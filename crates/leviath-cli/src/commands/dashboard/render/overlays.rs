//! Toast notifications, help overlay, and confirmation popup rendering.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

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

    /// The page-scoped help overlay, built from `(key, description)` tables
    /// via the shared builder so it cannot drift from the bindings.
    pub(in crate::commands::dashboard) fn draw_help_overlay(&self, frame: &mut Frame) {
        use crate::tui::widgets::help::{HelpSection, draw_help};
        let sections: Vec<HelpSection> = if self.mcp_screen {
            vec![HelpSection {
                title: "MCP servers",
                entries: vec![
                    ("↑ ↓ / k j", "move"),
                    ("a", "add a server"),
                    ("d", "remove (asks first)"),
                    ("l", "browser login"),
                    ("t", "test the connection"),
                    ("r", "refresh the list"),
                    ("esc", "back to the run list"),
                    ("q / ctrl-c", "quit"),
                ],
            }]
        } else if self.detail_view {
            vec![
                HelpSection {
                    title: "Detail view",
                    entries: vec![
                        ("← →", "switch stage tab"),
                        ("1-9", "jump to stage by number"),
                        ("↑ ↓ / k j", "scroll (review doc when shown)"),
                        ("pgup / pgdn", "scroll 10 lines"),
                        ("home / end (b / e)", "jump to begin / end"),
                        ("o / l / c", "output / logs / context"),
                        ("g", "stage explorer (graph agents)"),
                        (", / .", "older / newer context point"),
                        ("enter / space", "fold / unfold (context tree)"),
                        ("[ / ]", "previous / next region (context)"),
                        ("/", "search; n / N step matches"),
                        ("y", "yank pane to the clipboard"),
                        ("drag", "select text; release copies"),
                        ("i", "respond / send input"),
                        ("x", "kill the run (asks first)"),
                        ("p / r", "pause / resume"),
                        ("esc", "clear search, then back"),
                    ],
                },
                HelpSection {
                    title: "While typing input",
                    entries: vec![
                        ("enter", "send response / message"),
                        ("alt+enter", "insert a newline"),
                        ("esc", "cancel input"),
                    ],
                },
            ]
        } else {
            vec![HelpSection {
                title: "Run list",
                entries: vec![
                    ("↑ ↓ / k j", "select a run"),
                    ("home / end (g / G)", "first / last run"),
                    ("enter", "open the detail view"),
                    ("tab", "focus the log panel (scrollable)"),
                    ("/", "filter runs"),
                    ("s", "cycle sort: started / activity / status"),
                    ("x", "kill the run (asks first)"),
                    ("d", "delete the run (asks first)"),
                    ("p / r", "pause / resume"),
                    ("m", "manage MCP servers"),
                    ("esc", "clear the filter"),
                    ("q / ctrl-c", "quit"),
                ],
            }]
        };
        draw_help(frame, frame.area(), &sections);
    }
}

#[cfg(test)]
mod tests {
    use crate::commands::dashboard::test_support::{make_test_dashboard, rendered_buffer};
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
            last_progress_at: None,
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
        let buf = rendered_buffer(&terminal);
        assert!(
            buf.trim().is_empty(),
            "no toasts should draw nothing: {buf}"
        );
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Agent completed"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Needs input"), "{buf}");
        assert!(buf.contains("Agent failed"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Run list"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Run list"), "{buf}");
    }

    #[test]
    fn the_delete_dialog_renders_over_the_frame() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-del-123", AgentDisplayStatus::Complete));
        dash.update_display_indices();
        dash.request_delete();
        let (_, dialog) = dash.pending_confirm.as_ref().expect("dialog open").clone();
        terminal
            .draw(|f| {
                dialog.draw(f, f.area());
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Delete run?"));
        assert!(text.contains("[ Cancel ]"));
    }

    // ─── Regression: help overlay must scope to the current page ──────────
    //
    // draw_help_overlay() must scope its sections to `self.detail_view`:
    // rendering both the "Main list" and "Detail view"/"Input" sections
    // regardless of page would show the user keybindings for a page they
    // aren't on.

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
        assert!(rendered.contains("Run list"));
        assert!(rendered.contains("cycle sort"));
        assert!(rendered.contains("kill the run (asks first)"));
        assert!(!rendered.contains("Detail view"));
        assert!(!rendered.contains("switch stage tab"));
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
        assert!(rendered.contains("switch stage tab"));
        assert!(rendered.contains("While typing input"));
        assert!(!rendered.contains("Run list"));
        assert!(!rendered.contains("cycle sort"));
    }

    #[test]
    fn draw_help_overlay_mcp_screen_shows_its_own_keys() {
        let backend = TestBackend::new(120, 50);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.mcp_screen = true;
        terminal
            .draw(|f| {
                dash.draw_help_overlay(f);
            })
            .unwrap();

        let rendered = rendered_buffer(&terminal);
        assert!(rendered.contains("MCP servers"));
        assert!(rendered.contains("remove (asks first)"));
    }
}
