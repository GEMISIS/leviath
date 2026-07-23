//! MCP management screen rendering.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table, TableState};

use crate::commands::dashboard::state::Dashboard;
use crate::commands::dashboard::theme::*;

impl Dashboard {
    pub(in crate::commands::dashboard) fn draw_mcp_screen(
        &mut self,
        frame: &mut Frame,
        area: Rect,
    ) {
        // Table, then a bottom bar that is either the add-line editor or the
        // key hints.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);

        self.draw_mcp_table(frame, chunks[0]);

        if self.mcp_add_mode {
            self.draw_mcp_add_bar(frame, chunks[1]);
        } else {
            self.draw_mcp_help_bar(frame, chunks[1]);
        }
    }

    fn draw_mcp_table(&self, frame: &mut Frame, area: Rect) {
        let header = Row::new(
            ["Name", "Transport", "Endpoint", "Auth"]
                .into_iter()
                .map(|h| {
                    Cell::from(Span::styled(
                        h,
                        Style::default().add_modifier(Modifier::BOLD),
                    ))
                }),
        );

        let rows: Vec<Row> = self
            .mcp_rows
            .iter()
            .map(|r| {
                Row::new(vec![
                    Cell::from(r.name.clone()),
                    Cell::from(r.transport.clone()),
                    Cell::from(r.endpoint.clone()),
                    Cell::from(Span::styled(r.auth.clone(), auth_style(&r.auth))),
                ])
            })
            .collect();

        let title = format!(" MCP Servers ({}) ", self.mcp_rows.len());
        let table = Table::new(
            rows,
            [
                Constraint::Percentage(22),
                Constraint::Percentage(14),
                Constraint::Percentage(48),
                Constraint::Percentage(16),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(title),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");

        let mut state = TableState::default();
        if !self.mcp_rows.is_empty() {
            state.select(Some(self.mcp_selected.min(self.mcp_rows.len() - 1)));
        }
        frame.render_stateful_widget(table, area, &mut state);

        // Empty-state hint drawn inside the table body.
        if self.mcp_rows.is_empty() {
            let hint = Paragraph::new(Line::from(Span::styled(
                "No MCP servers. Press 'a' to add one.",
                Style::default().fg(C_DIM),
            )));
            let inner = Rect {
                x: area.x + 2,
                y: area.y + 2,
                width: area.width.saturating_sub(4),
                height: 1,
            };
            frame.render_widget(hint, inner);
        }
    }

    fn draw_mcp_add_bar(&self, frame: &mut Frame, area: Rect) {
        let line = Line::from(vec![
            Span::styled(
                "add › ",
                Style::default().fg(C_ACTIVE).add_modifier(Modifier::BOLD),
            ),
            Span::raw(self.mcp_add_input.as_str()),
            // A block cursor.
            Span::styled("█", Style::default().fg(C_ACTIVE)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn draw_mcp_help_bar(&self, frame: &mut Frame, area: Rect) {
        let hint = Paragraph::new(Line::from(Span::styled(
            " ↑↓ select · a add · d delete · l login · t test · r refresh · q back ",
            Style::default().fg(C_DIM),
        )));
        frame.render_widget(hint, area);
    }
}

/// Colour a server's auth state.
fn auth_style(auth: &str) -> Style {
    match auth {
        "authenticated" => Style::default().fg(C_SUCCESS),
        "expired" => Style::default().fg(C_WARN),
        "none" => Style::default().fg(C_ERROR),
        _ => Style::default().fg(C_DIM),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::dashboard::test_support::make_test_dashboard;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn rendered(dash: &mut Dashboard) -> String {
        rendered_sized(dash, 100, 20)
    }

    fn rendered_sized(dash: &mut Dashboard, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| dash.draw(f)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
    }

    #[test]
    fn empty_screen_shows_the_add_hint() {
        let mut dash = make_test_dashboard();
        dash.mcp_screen = true;
        let out = rendered(&mut dash);
        assert!(out.contains("MCP Servers (0)"), "got: {out}");
        assert!(out.contains("Press 'a' to add"), "got: {out}");
        assert!(out.contains("a add"), "help bar: {out}");
    }

    #[test]
    fn screen_lists_servers_with_each_auth_colour() {
        use crate::config::Config;
        use leviath_mcp::MCPServerConfig;
        let dir = tempfile::tempdir().unwrap();
        let mut dash = make_test_dashboard();
        dash.mcp_ctx.config_path = dir.path().join("config.toml");
        dash.mcp_ctx.store_path = dir.path().join("mcp-auth.json");
        // Seed config directly so no toast is queued (a toast would overlay the
        // top-right auth column we are asserting on).
        let mut config = Config::default();
        config
            .mcp_servers
            .push(MCPServerConfig::http("remote", "https://e.com/mcp"));
        config
            .mcp_servers
            .push(MCPServerConfig::stdio("local", "npx", vec![]));
        config
            .save_to_path_public(&dash.mcp_ctx.config_path)
            .unwrap();
        // Seed an authenticated token to exercise that style.
        let mut store = leviath_mcp::AuthStore::default();
        store.set(
            "remote",
            leviath_mcp::ServerAuth {
                expires_at: 10_000,
                ..Default::default()
            },
        );
        store.save(&dash.mcp_ctx.store_path).unwrap();
        dash.mcp_screen = true;
        dash.refresh_mcp_rows();

        // Wide enough that the auth column is not truncated.
        let out = rendered_sized(&mut dash, 160, 20);
        assert!(out.contains("MCP Servers (2)"), "got: {out}");
        assert!(out.contains("remote"));
        assert!(out.contains("local"));
        assert!(out.contains("authenticated"), "auth column: {out}");
    }

    #[test]
    fn add_mode_shows_the_line_editor() {
        let mut dash = make_test_dashboard();
        dash.mcp_screen = true;
        dash.mcp_add_mode = true;
        dash.mcp_add_input = "navigator https://x".to_string();
        let out = rendered(&mut dash);
        assert!(out.contains("add ›"), "got: {out}");
        assert!(out.contains("navigator https://x"), "got: {out}");
    }

    #[test]
    fn auth_style_distinguishes_states() {
        assert_eq!(auth_style("authenticated").fg, Some(C_SUCCESS));
        assert_eq!(auth_style("expired").fg, Some(C_WARN));
        assert_eq!(auth_style("none").fg, Some(C_ERROR));
        assert_eq!(auth_style("n/a").fg, Some(C_DIM));
    }
}
