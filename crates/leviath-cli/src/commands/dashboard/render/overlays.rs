//! Toast notifications, help overlay, and confirmation popup rendering.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};

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

        // Only show keybindings relevant to the page the user is currently on.
        let mut lines = if self.detail_view {
            self.detail_view_help_lines()
        } else {
            self.main_list_help_lines()
        };
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Any key to dismiss",
            Style::default().fg(C_DIM),
        )));

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

    fn main_list_help_lines(&self) -> Vec<Line<'static>> {
        vec![
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
                    "  m        ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Manage MCP servers"),
            ]),
            Line::from(vec![
                Span::styled(
                    "  Esc      ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Clear filter / Quit"),
            ]),
        ]
    }

    fn detail_view_help_lines(&self) -> Vec<Line<'static>> {
        vec![
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
                    "  drag     ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Select text; release copies (Shift+drag: terminal select)"),
            ]),
            Line::from(vec![
                Span::styled(
                    "  i        ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Respond / provide input (when supported)"),
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
                "  Input",
                Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "  Enter    ",
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                Span::raw("Send response / message"),
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
        ]
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

#[cfg(test)]
mod tests {
    use crate::commands::dashboard::test_support::make_test_dashboard;
    use crate::commands::dashboard::types::*;
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

    #[test]
    fn draw_toasts_empty() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        terminal
            .draw(|f| {
                dash.draw_toasts(f);
            })
            .unwrap();
    }

    #[test]
    fn draw_toasts_info() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.toasts.push(Toast {
            message: "Agent completed".to_string(),
            remaining_ticks: 25,
            level: ToastLevel::Info,
        });
        terminal
            .draw(|f| {
                dash.draw_toasts(f);
            })
            .unwrap();
    }

    #[test]
    fn draw_toasts_warning_and_error() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.toasts.push(Toast {
            message: "Needs input".to_string(),
            remaining_ticks: 25,
            level: ToastLevel::Warning,
        });
        dash.toasts.push(Toast {
            message: "Agent failed".to_string(),
            remaining_ticks: 25,
            level: ToastLevel::Error,
        });
        terminal
            .draw(|f| {
                dash.draw_toasts(f);
            })
            .unwrap();
    }

    #[test]
    fn draw_help_overlay_renders() {
        let backend = TestBackend::new(120, 50);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        terminal
            .draw(|f| {
                dash.draw_help_overlay(f);
            })
            .unwrap();
    }

    #[test]
    fn draw_help_overlay_small_terminal() {
        let backend = TestBackend::new(40, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        terminal
            .draw(|f| {
                dash.draw_help_overlay(f);
            })
            .unwrap();
    }

    #[test]
    fn draw_confirm_popup_with_agent() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-del-123", AgentDisplayStatus::Complete));
        dash.update_display_indices();
        dash.confirm_delete = true;
        terminal
            .draw(|f| {
                dash.draw_confirm_popup(f);
            })
            .unwrap();
    }

    #[test]
    fn draw_confirm_popup_no_agent() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        terminal
            .draw(|f| {
                dash.draw_confirm_popup(f);
            })
            .unwrap();
    }

    // ─── Regression: help overlay must scope to the current page ──────────
    //
    // draw_help_overlay() must scope its sections to `self.detail_view`:
    // rendering both the "Main list" and "Detail view"/"Input" sections
    // regardless of page would show the user keybindings for a page they
    // aren't on.

    fn rendered_buffer(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn draw_help_overlay_main_list_omits_detail_view_section() {
        let backend = TestBackend::new(120, 50);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.detail_view = false;
        terminal
            .draw(|f| {
                dash.draw_help_overlay(f);
            })
            .unwrap();

        let rendered = rendered_buffer(&terminal);
        assert!(rendered.contains("Main list"));
        assert!(!rendered.contains("Detail view"));
        assert!(!rendered.contains("Switch stage tab"));
    }

    #[test]
    fn draw_help_overlay_detail_view_omits_main_list_section() {
        let backend = TestBackend::new(120, 50);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.detail_view = true;
        terminal
            .draw(|f| {
                dash.draw_help_overlay(f);
            })
            .unwrap();

        let rendered = rendered_buffer(&terminal);
        assert!(rendered.contains("Detail view"));
        assert!(rendered.contains("Switch stage tab"));
        assert!(!rendered.contains("Main list"));
        assert!(!rendered.contains("Select agent"));
    }
}
