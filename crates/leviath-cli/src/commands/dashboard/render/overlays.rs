//! Toast notifications, help overlay, and confirmation popup rendering.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use ratatui::Frame;

use crate::commands::dashboard::helpers::truncate;
use crate::commands::dashboard::state::Dashboard;
use crate::commands::dashboard::theme::*;

impl Dashboard {
    pub(in crate::commands::dashboard) fn draw_toasts(&self, frame: &mut Frame) {
        if self.toasts.is_empty() {
            return;
        }
        let area = frame.area();
        let toast_w: u16 = 40;
        let toast_h: u16 = self.toasts.len() as u16;
        let x = area.width.saturating_sub(toast_w + 1);
        let y: u16 = 1;
        let toast_area = Rect {
            x,
            y,
            width: toast_w,
            height: toast_h,
        };
        frame.render_widget(Clear, toast_area);
        for (i, toast) in self.toasts.iter().enumerate() {
            let color = match toast.level {
                super::super::types::ToastLevel::Info => C_SUCCESS,
                super::super::types::ToastLevel::Warning => C_WARN,
                super::super::types::ToastLevel::Error => C_ERROR,
            };
            let icon = match toast.level {
                super::super::types::ToastLevel::Info => "✓",
                super::super::types::ToastLevel::Warning => "⏸",
                super::super::types::ToastLevel::Error => "✗",
            };
            let msg = truncate(&toast.message, (toast_w - 4) as usize);
            let line = Line::from(vec![
                Span::styled(
                    format!(" {} ", icon),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(msg, Style::default().fg(C_WHITE)),
            ]);
            let row = Rect {
                x,
                y: y + i as u16,
                width: toast_w,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(line).style(Style::default().bg(Color::Rgb(30, 30, 30))),
                row,
            );
        }
    }

    pub(in crate::commands::dashboard) fn draw_help_overlay(&self, frame: &mut Frame) {
        let area = frame.area();
        let w: u16 = 62.min(area.width.saturating_sub(4));
        let h: u16 = 38.min(area.height.saturating_sub(4));
        let x = (area.width.saturating_sub(w)) / 2;
        let y = (area.height.saturating_sub(h)) / 2;
        let popup = Rect {
            x,
            y,
            width: w,
            height: h,
        };
        frame.render_widget(Clear, popup);

        let lines: Vec<Line> = vec![
            Line::from(Span::styled(
                "  Main list",
                Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "  ↑/↓      ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Select agent (sorted: active first)"),
            ]),
            Line::from(vec![
                Span::styled(
                    "  Enter    ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Open detail view"),
            ]),
            Line::from(vec![
                Span::styled(
                    "  /        ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Filter agents by name/status"),
            ]),
            Line::from(vec![
                Span::styled(
                    "  d        ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Delete run (permanent)"),
            ]),
            Line::from(vec![
                Span::styled(
                    "  c / k    ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Cancel / Kill agent"),
            ]),
            Line::from(vec![
                Span::styled(
                    "  Esc      ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Clear filter / Quit"),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  Detail view",
                Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "  ←/→      ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Switch stage tab"),
            ]),
            Line::from(vec![
                Span::styled(
                    "  1-9      ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Jump to stage by number"),
            ]),
            Line::from(vec![
                Span::styled(
                    "  ↑/↓      ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Scroll output (review doc when shown)"),
            ]),
            Line::from(vec![
                Span::styled(
                    "  PgUp/Dn  ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Scroll 10 lines"),
            ]),
            Line::from(vec![
                Span::styled(
                    "  b / e    ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Jump to begin / end"),
            ]),
            Line::from(vec![
                Span::styled(
                    "  l / o    ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Toggle Logs / Output"),
            ]),
            Line::from(vec![
                Span::styled(
                    "  /        ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Search output/logs"),
            ]),
            Line::from(vec![
                Span::styled(
                    "  n / N    ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Next / previous search match"),
            ]),
            Line::from(vec![
                Span::styled(
                    "  y        ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Yank output/logs to clipboard (OSC52)"),
            ]),
            Line::from(vec![
                Span::styled(
                    "  i        ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Respond (when input needed)"),
            ]),
            Line::from(vec![
                Span::styled(
                    "  k        ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Kill agent"),
            ]),
            Line::from(vec![
                Span::styled(
                    "  Esc      ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Clear search / back to list"),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  Input (text response)",
                Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "  Enter    ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Send response"),
            ]),
            Line::from(vec![
                Span::styled(
                    "  Alt+↵    ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Insert newline (multi-line)"),
            ]),
            Line::from(vec![
                Span::styled(
                    "  Esc      ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Cancel input"),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  Any key to dismiss",
                Style::default().fg(C_DIM),
            )),
        ];

        let widget = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(C_ACCENT))
                .title(Span::styled(
                    " Help  ? ",
                    Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
                ))
                .padding(Padding::uniform(0)),
        );
        frame.render_widget(widget, popup);
    }

    pub(in crate::commands::dashboard) fn draw_confirm_popup(&self, frame: &mut Frame) {
        let area = frame.area();
        let w: u16 = 56.min(area.width.saturating_sub(4));
        let h: u16 = 5;
        let x = (area.width.saturating_sub(w)) / 2;
        let y = (area.height.saturating_sub(h)) / 2;
        let popup = Rect {
            x,
            y,
            width: w,
            height: h,
        };
        frame.render_widget(Clear, popup);

        let agent_id = self.selected_agent().map(|a| a.id.as_str()).unwrap_or("?");
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!(
                    "  Delete run '{}'?  This is permanent.",
                    truncate(agent_id, 24)
                ),
                Style::default().fg(C_WARN),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "  [y]",
                    Style::default().fg(C_ERROR).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" confirm  "),
                Span::styled("[any key]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" cancel"),
            ]),
        ];
        let widget = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(C_ERROR))
                .title(Span::styled(
                    " Confirm Delete ",
                    Style::default().fg(C_ERROR).add_modifier(Modifier::BOLD),
                )),
        );
        frame.render_widget(widget, popup);
    }
}
