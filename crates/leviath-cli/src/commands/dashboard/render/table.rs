//! Agent table, log panel, and help bar rendering.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table};
use ratatui::Frame;

use crate::commands::dashboard::helpers::{format_tokens, relative_time, truncate};
use crate::commands::dashboard::state::Dashboard;
use crate::commands::dashboard::theme::*;
use crate::commands::dashboard::types::*;
use crate::interaction;

impl Dashboard {
    pub(in crate::commands::dashboard) fn draw_agent_table(
        &mut self,
        frame: &mut Frame,
        area: Rect,
    ) {
        let header = Row::new(vec![
            Cell::from(Span::styled(
                "Title / ID",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Cell::from(Span::styled(
                "Agent",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Cell::from(Span::styled(
                "Stage",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Cell::from(Span::styled(
                "Status",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Cell::from(Span::styled(
                "Tokens",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Cell::from(Span::styled(
                "Started",
                Style::default().add_modifier(Modifier::BOLD),
            )),
        ])
        .style(Style::default().fg(C_MUTED))
        .height(1);

        let spinner_frame = SPINNER[(self.tick_count as usize) % SPINNER.len()];

        let rows: Vec<Row> = self
            .display_indices
            .iter()
            .map(|&idx| {
                let agent = &self.agents[idx];
                let status_color = agent.status.color();
                let started_str = relative_time(agent.started_at);
                let title_str = agent
                    .title
                    .as_deref()
                    .map(|t| truncate(t.trim_start_matches('#').trim(), 26))
                    .unwrap_or_else(|| truncate(&agent.task, 26));
                let tok_str = if agent.is_run_state {
                    if agent.tokens_in == 0 && agent.tokens_out == 0 {
                        "—".to_string()
                    } else {
                        format!(
                            "{}↑ {}↓",
                            format_tokens(agent.tokens_in),
                            format_tokens(agent.tokens_out)
                        )
                    }
                } else {
                    let (cur, max) = agent.context_tokens;
                    if max > 0 {
                        format!("{}/{}", format_tokens(cur), format_tokens(max))
                    } else {
                        format_tokens(cur)
                    }
                };
                let stage_str = if agent.num_stages > 1 {
                    format!(
                        "{} {}/{}",
                        truncate(&agent.stage, 10),
                        agent.stage_index + 1,
                        agent.num_stages
                    )
                } else {
                    truncate(&agent.stage, 14)
                };
                let status_str = if matches!(agent.status, AgentDisplayStatus::Active) {
                    format!("{} ACTIVE", spinner_frame)
                } else {
                    agent.status.to_string()
                };
                let short_id = agent.id.split('-').next_back().unwrap_or("").to_string();
                let title_cell = Cell::from(Line::from(vec![
                    Span::styled(title_str, Style::default().fg(C_WHITE)),
                    Span::styled(format!(" #{}", short_id), Style::default().fg(C_DIM)),
                ]));
                Row::new(vec![
                    title_cell,
                    Cell::from(agent.blueprint_name.clone()),
                    Cell::from(stage_str),
                    Cell::from(status_str).style(Style::default().fg(status_color)),
                    Cell::from(tok_str),
                    Cell::from(started_str).style(Style::default().fg(C_DIM)),
                ])
            })
            .collect();

        let empty_state_msg: Option<String> = if self.agents.is_empty() {
            Some("  No agents running. Use `lev run <agent>` to start one.".to_string())
        } else if self.display_indices.is_empty() {
            Some(format!("  No agents match \"{}\".", self.list_search_query))
        } else {
            None
        };

        let list_title = if !self.list_search_query.is_empty() {
            format!(
                " Agents  /{}/  {}/{} ",
                self.list_search_query,
                self.display_indices.len(),
                self.agents.len()
            )
        } else if self.list_search_mode {
            format!(" Agents  /{}▌ ", self.list_search_query)
        } else {
            " Agents ".to_string()
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(C_BORDER))
            .title(Span::styled(
                list_title,
                Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
            ));

        if let Some(msg) = empty_state_msg {
            let widget = Paragraph::new(Line::from(Span::styled(msg, Style::default().fg(C_DIM))))
                .block(block);
            frame.render_widget(widget, area);
            return;
        }

        let table = Table::new(
            rows,
            [
                ratatui::layout::Constraint::Percentage(22),
                ratatui::layout::Constraint::Percentage(12),
                ratatui::layout::Constraint::Percentage(14),
                ratatui::layout::Constraint::Percentage(18),
                ratatui::layout::Constraint::Percentage(14),
                ratatui::layout::Constraint::Percentage(20),
            ],
        )
        .header(header)
        .block(block)
        .row_highlight_style(
            Style::default()
                .add_modifier(Modifier::REVERSED)
                .fg(C_WHITE),
        );

        frame.render_stateful_widget(table, area, &mut self.table_state);
    }

    pub(in crate::commands::dashboard) fn draw_log_panel(&self, frame: &mut Frame, area: Rect) {
        let log_lines: Vec<Line> = self
            .log
            .iter()
            .rev()
            .take(area.height.saturating_sub(2) as usize)
            .rev()
            .map(|entry| {
                Line::from(vec![
                    Span::styled(format!(" {} ", entry.timestamp), Style::default().fg(C_DIM)),
                    Span::styled(&entry.message, Style::default().fg(C_MUTED)),
                ])
            })
            .collect();

        let log = Paragraph::new(log_lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(C_BORDER))
                .title(Span::styled(" Log ", Style::default().fg(C_DIM))),
        );
        frame.render_widget(log, area);
    }

    pub(in crate::commands::dashboard) fn draw_help_bar(&self, frame: &mut Frame, area: Rect) {
        use interaction::InteractionKind;

        let help = if self.confirm_delete {
            Line::from(vec![
                Span::styled(
                    "[y]",
                    Style::default().fg(C_ERROR).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" confirm delete  "),
                Span::styled("[any key]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" cancel"),
            ])
        } else if self.detail_view && self.input_mode {
            let kind = self
                .selected_agent()
                .and_then(|a| a.pending_request.as_ref())
                .map(|r| r.kind.clone());
            match kind {
                Some(InteractionKind::FreeText) | None => Line::from(vec![
                    Span::styled(
                        "[Enter]",
                        Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" send  "),
                    Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" cancel"),
                ]),
                _ => Line::from(vec![
                    Span::styled(
                        "[↑↓]",
                        Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" select  "),
                    Span::styled(
                        "[Enter]",
                        Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" confirm  "),
                    Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" cancel"),
                ]),
            }
        } else if self.detail_view && self.search_mode {
            Line::from(vec![
                Span::styled(" Search: /", Style::default().fg(C_ACCENT)),
                Span::raw(self.search_query.clone()),
                Span::styled("▌", Style::default().fg(C_ACCENT)),
                Span::raw("  "),
                Span::styled(
                    "[Enter]",
                    Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" confirm  "),
                Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" cancel"),
            ])
        } else if self.detail_view && !self.search_query.is_empty() {
            Line::from(vec![
                Span::styled(
                    format!(" /{}/", self.search_query),
                    Style::default().fg(C_ACCENT),
                ),
                Span::raw("  "),
                Span::styled(
                    "[n]",
                    Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" next  "),
                Span::styled("[N]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" prev  "),
                Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" clear search  "),
                Span::styled("[y]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" yank  "),
                Span::styled(
                    "[?]",
                    Style::default().fg(C_DIM).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" help"),
            ])
        } else if self.detail_view {
            self.build_detail_help_bar()
        } else if self.list_search_mode {
            Line::from(vec![
                Span::styled(" Filter: /", Style::default().fg(C_ACCENT)),
                Span::raw(self.list_search_query.clone()),
                Span::styled("▌", Style::default().fg(C_ACCENT)),
                Span::raw("  "),
                Span::styled(
                    "[Enter]",
                    Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" confirm  "),
                Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" clear"),
            ])
        } else if !self.list_search_query.is_empty() {
            Line::from(vec![
                Span::styled(
                    format!(
                        " /{}/  {}/{} ",
                        self.list_search_query,
                        self.display_indices.len(),
                        self.agents.len()
                    ),
                    Style::default().fg(C_ACCENT),
                ),
                Span::styled("[/]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" refine  "),
                Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" clear  "),
                Span::styled(
                    "[Enter]",
                    Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" detail  "),
                Span::styled(
                    "[?]",
                    Style::default().fg(C_DIM).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" help"),
            ])
        } else {
            self.build_main_list_help_bar()
        };

        let help_widget = Paragraph::new(help).style(Style::default().bg(Color::Rgb(20, 20, 30)));
        frame.render_widget(help_widget, area);
    }

    fn build_detail_help_bar(&self) -> Line<'static> {
        let can_respond = self.selected_stage_can_respond();
        let accepts_messages = self.selected_agent_accepts_messages();
        let can_kill = self
            .selected_agent()
            .map(|a| {
                matches!(
                    a.status,
                    AgentDisplayStatus::Active | AgentDisplayStatus::Waiting
                )
            })
            .unwrap_or(false);
        let mut spans = vec![
            Span::styled(
                "[←/→]",
                Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" stage  "),
            Span::styled("[↑/↓]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" scroll  "),
            Span::styled("[/]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" search  "),
            Span::styled("[y]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" yank  "),
            Span::styled("[l/o/c]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" logs/out/ctx  "),
        ];
        if can_respond || accepts_messages {
            spans.push(Span::styled(
                "[i]",
                Style::default().fg(C_WARN).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(if can_respond {
                " respond  "
            } else {
                " input  "
            }));
        }
        if can_kill {
            spans.push(Span::styled(
                "[k]",
                Style::default().fg(C_ERROR).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" kill  "));
        }
        spans.push(Span::styled(
            "[?]",
            Style::default().fg(C_DIM).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" help  "));
        spans.push(Span::styled(
            "[Esc]",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" back"));
        Line::from(spans)
    }

    fn build_main_list_help_bar(&self) -> Line<'static> {
        let can_kill = self
            .selected_agent()
            .map(|a| {
                matches!(
                    a.status,
                    AgentDisplayStatus::Active | AgentDisplayStatus::Waiting
                )
            })
            .unwrap_or(false);
        let mut spans = vec![
            Span::styled(
                "[↑↓]",
                Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" select  "),
            Span::styled(
                "[Enter]",
                Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" detail  "),
            Span::styled(
                "[/]",
                Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" filter  "),
            Span::styled("[d]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" delete  "),
        ];
        if can_kill {
            spans.push(Span::styled(
                "[c]",
                Style::default().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" cancel  "));
            spans.push(Span::styled(
                "[k]",
                Style::default().fg(C_ERROR).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" kill  "));
        }
        spans.push(Span::styled(
            "[?]",
            Style::default().fg(C_DIM).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" help  "));
        spans.push(Span::styled(
            "[Esc]",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" quit"));
        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use tokio::sync::mpsc;

    fn make_test_dashboard() -> Dashboard {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        Dashboard::new(cmd_tx)
    }

    fn make_test_agent(id: &str, status: AgentDisplayStatus) -> DashboardAgent {
        DashboardAgent {
            id: id.to_string(),
            blueprint_name: "test-agent".to_string(),
            agent_path: "/path".to_string(),
            stage: "main".to_string(),
            stage_index: 0,
            num_stages: 2,
            status,
            tokens_in: 100,
            tokens_out: 50,
            cached_tokens: 10,
            context_tokens: (500, 8000),
            iteration: 3,
            waiting_prompt: None,
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
    fn draw_agent_table_empty() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_agent_table(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_agent_table_filter_no_match() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.list_search_query = "nonexistent".to_string();
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_agent_table(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_agent_table_multiple_agents() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.agents
            .push(make_test_agent("run-2", AgentDisplayStatus::Complete));
        dash.agents
            .push(make_test_agent("run-3", AgentDisplayStatus::Waiting));
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_agent_table(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_agent_table_non_run_state() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-ecs", AgentDisplayStatus::Active);
        agent.is_run_state = false;
        agent.context_tokens = (4000, 8000);
        dash.agents.push(agent);
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_agent_table(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_agent_table_run_state_zero_tokens_shows_dash() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-zero-tok", AgentDisplayStatus::Active);
        agent.is_run_state = true;
        agent.tokens_in = 0;
        agent.tokens_out = 0;
        dash.agents.push(agent);
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_agent_table(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_agent_table_title_none_falls_back_to_task() {
        // `title` is `None` before title generation completes (or when
        // disabled) -- exercises the `.unwrap_or_else(|| truncate(&agent.task, 26))`
        // fallback closure, which every other test in this file never
        // reaches because `make_test_agent` always sets `title: Some(...)`.
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-no-title", AgentDisplayStatus::Active);
        agent.title = None;
        agent.task = "fallback task text".to_string();
        dash.agents.push(agent);
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_agent_table(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_agent_table_single_stage_omits_stage_counter() {
        // `make_test_agent` defaults to `num_stages: 2`, always taking the
        // `agent.num_stages > 1` branch (stage name + "i/n" counter). A
        // single-stage agent takes the other arm (bare truncated stage
        // name, no counter) -- never exercised elsewhere in this file.
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-single-stage", AgentDisplayStatus::Active);
        agent.num_stages = 1;
        dash.agents.push(agent);
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_agent_table(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_agent_table_non_run_state_zero_max_context() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-zero-ctx", AgentDisplayStatus::Active);
        agent.is_run_state = false;
        agent.context_tokens = (0, 0);
        dash.agents.push(agent);
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_agent_table(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_agent_table_with_list_search_mode() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.list_search_mode = true;
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_agent_table(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_log_panel_empty() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.log.clear();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_log_panel(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_log_panel_with_entries() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.log.clear();
        dash.log.push(LogEntry {
            timestamp: "12:00:00".to_string(),
            message: "Agent started".to_string(),
        });
        dash.log.push(LogEntry {
            timestamp: "12:00:01".to_string(),
            message: "Stage changed to implement".to_string(),
        });
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_log_panel(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_help_bar_main_list_mode() {
        let backend = TestBackend::new(120, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.draw_help_bar(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_help_bar_detail_view() {
        let backend = TestBackend::new(120, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.detail_view = true;
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.draw_help_bar(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_help_bar_input_mode_freetext() {
        let backend = TestBackend::new(120, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.detail_view = true;
        dash.input_mode = true;
        // No pending request means FreeText fallback
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.draw_help_bar(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_help_bar_input_mode_choice() {
        let backend = TestBackend::new(120, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-choice", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(interaction::InteractionRequest {
            id: "req-1".to_string(),
            kind: interaction::InteractionKind::MultipleChoice,
            prompt: "Pick one".to_string(),
            options: vec!["A".to_string(), "B".to_string()],
            tool_name: None,
            tool_arguments: None,
            required: true,
            stage_name: "main".to_string(),
            body: None,
            body_format: interaction::BodyFormat::Plain,
        });
        agent.waiting_prompt = Some("Pick one".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.draw_help_bar(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_help_bar_search_mode() {
        let backend = TestBackend::new(120, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.detail_view = true;
        dash.search_mode = true;
        dash.search_query = "hello".to_string();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.draw_help_bar(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_help_bar_search_active_not_mode() {
        let backend = TestBackend::new(120, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.detail_view = true;
        dash.search_mode = false;
        dash.search_query = "test".to_string();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.draw_help_bar(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_help_bar_confirm_delete() {
        let backend = TestBackend::new(120, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.confirm_delete = true;
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.draw_help_bar(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_help_bar_list_search_mode() {
        let backend = TestBackend::new(120, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.list_search_mode = true;
        dash.list_search_query = "cod".to_string();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.draw_help_bar(f, area);
            })
            .unwrap();
    }

    #[test]
    fn draw_help_bar_list_filter_active() {
        let backend = TestBackend::new(120, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.list_search_mode = false;
        dash.list_search_query = "coder".to_string();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.draw_help_bar(f, area);
            })
            .unwrap();
    }

    #[test]
    fn build_detail_help_bar_with_can_respond() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-resp", AgentDisplayStatus::Waiting);
        agent.waiting_prompt = Some("Do something".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.selected_stage = 0;
        let line = dash.build_detail_help_bar();
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("respond"));
    }

    #[test]
    fn build_detail_help_bar_with_can_kill() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-kill", AgentDisplayStatus::Active));
        dash.update_display_indices();
        let line = dash.build_detail_help_bar();
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("kill"));
    }

    #[test]
    fn build_detail_help_bar_complete_no_kill() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-done", AgentDisplayStatus::Complete));
        dash.update_display_indices();
        let line = dash.build_detail_help_bar();
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains("kill"));
    }

    #[test]
    fn build_main_list_help_bar_basic() {
        let dash = make_test_dashboard();
        let line = dash.build_main_list_help_bar();
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("select"));
        assert!(text.contains("detail"));
        assert!(text.contains("quit"));
    }

    #[test]
    fn build_main_list_help_bar_with_killable() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-k", AgentDisplayStatus::Active));
        dash.update_display_indices();
        let line = dash.build_main_list_help_bar();
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("kill"));
        assert!(text.contains("cancel"));
    }

    #[test]
    fn build_main_list_help_bar_selected_complete_no_kill() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-done", AgentDisplayStatus::Complete));
        dash.update_display_indices();
        let line = dash.build_main_list_help_bar();
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("select"));
        assert!(!text.contains("kill"));
    }

    #[test]
    fn build_detail_help_bar_with_accepts_messages() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-msg", AgentDisplayStatus::Active);
        agent.accepts_messages = true;
        dash.agents.push(agent);
        dash.update_display_indices();
        let line = dash.build_detail_help_bar();
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("input"));
        assert!(text.contains("[i]"));
        // 'm' was unified into 'i' — no separate [m] hint should remain.
        assert!(!text.contains("[m]"));
    }

    #[test]
    fn build_detail_help_bar_can_respond_shows_respond_label() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-resp", AgentDisplayStatus::Waiting);
        agent.waiting_prompt = Some("prompt".to_string());
        agent.pending_request = Some(crate::interaction::InteractionRequest::free_text(
            "ft1", "prompt", "main", true,
        ));
        agent.stage_index = 0;
        agent.stages = vec![crate::runstate::StageRecord::new("main".to_string(), 0)];
        dash.agents.push(agent);
        dash.update_display_indices();
        let line = dash.build_detail_help_bar();
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("[i]"));
        assert!(text.contains("respond"));
    }

    #[test]
    fn build_detail_help_bar_no_input_hint_when_neither() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-none", AgentDisplayStatus::Active);
        agent.accepts_messages = false;
        dash.agents.push(agent);
        dash.update_display_indices();
        let line = dash.build_detail_help_bar();
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains("[i]"));
    }
}
