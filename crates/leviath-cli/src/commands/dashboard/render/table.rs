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
        if can_respond {
            spans.push(Span::styled(
                "[i]",
                Style::default().fg(C_WARN).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" respond  "));
        }
        if self.selected_agent_accepts_messages() {
            spans.push(Span::styled(
                "[m]",
                Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" message  "));
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
