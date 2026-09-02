//! Agent table, log panel, and help bar rendering.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Paragraph, Row, Table};
use unicode_width::UnicodeWidthStr;

use crate::commands::dashboard::helpers::{fit_parts, format_tokens, relative_time, truncate};
use crate::commands::dashboard::state::Dashboard;
use crate::commands::dashboard::theme::*;
use crate::commands::dashboard::types::*;
use leviath_core::interaction;

/// The run table's columns, as shares of the width inside its border.
const TABLE_COLUMNS: [Constraint; 6] = [
    Constraint::Percentage(22),
    Constraint::Percentage(12),
    Constraint::Percentage(14),
    Constraint::Percentage(18),
    Constraint::Percentage(14),
    Constraint::Percentage(20),
];

/// The column widths the table will draw with, in columns, for a table drawn
/// over `area`: the same layout the widget runs, over the same inner width,
/// with its default one-column spacing. A cell longer than its column is
/// clipped at the column edge with no sign that anything is missing, so each
/// text cell is cut to this width with an ellipsis before it is handed over.
fn table_column_widths(area: Rect) -> Vec<usize> {
    let inner = Rect::new(0, 0, area.width.saturating_sub(2), 1);
    Layout::horizontal(TABLE_COLUMNS)
        .spacing(1)
        .split(inner)
        .iter()
        .map(|r| r.width as usize)
        .collect()
}

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
                "Blueprint",
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

        // A mark column appears only while at least one run is marked, so the
        // table looks exactly as before until the feature is used.
        let any_marked = !self.marked.is_empty();
        let col_w = table_column_widths(area);

        let rows: Vec<Row> = self
            .display_indices
            .iter()
            .enumerate()
            .map(|(pos, &idx)| {
                let agent = &self.agents[idx];
                // Tree shape (parent → child nesting); default when flat.
                let tree = self.tree_rows.get(pos).cloned().unwrap_or_default();
                // A parked run only wears the attention colour when a person is
                // actually needed. A parent whose workers are still running is
                // healthy, and colouring it like a question is what taught
                // people to ignore the colour.
                let status_color = match &agent.wait_reason {
                    Some(reason) if !reason.needs_a_person() => C_DIM,
                    _ => agent.status.color(),
                };
                let started_str = relative_time(agent.started_at);
                let title_str = agent
                    .title
                    .as_deref()
                    .map(|t| t.trim_start_matches('#').trim().to_string())
                    .unwrap_or_else(|| agent.task.trim().to_string());
                let tok_str = if agent.tokens_in == 0 && agent.tokens_out == 0 {
                    "-".to_string()
                } else {
                    format!(
                        "{}↑ {}↓",
                        format_tokens(agent.tokens_in),
                        format_tokens(agent.tokens_out)
                    )
                };
                // The stage name gives way to its counter, and the whole cell
                // to its column.
                let stage_str = if agent.num_stages > 1 {
                    fit_parts(
                        &[
                            agent.stage.clone(),
                            format!(" {}/{}", agent.stage_index + 1, agent.num_stages),
                        ],
                        col_w[2],
                        &[0],
                    )
                    .concat()
                } else {
                    truncate(&agent.stage, col_w[2])
                };
                let status_str = match (&agent.status, &agent.wait_reason) {
                    (AgentDisplayStatus::Active, _) => format!("{} ACTIVE", spinner_frame),
                    // The reason replaces the word rather than following it.
                    // "WAITING" already means "parked", so it is the half that
                    // says nothing, and this column is too narrow to carry
                    // both without truncating the half that does.
                    (AgentDisplayStatus::Waiting, Some(reason)) => {
                        format!("{GLYPH_WAITING}{reason}")
                    }
                    // A run parked until the machine is fixed carries a reason
                    // too, and it is the one row where a bare PAUSED would be
                    // actively misleading: it reads as somebody's own decision
                    // rather than something waiting on them.
                    (AgentDisplayStatus::Paused, Some(reason)) => {
                        format!("{GLYPH_PENDING}{reason}")
                    }
                    (status, _) => status.to_string(),
                };
                let short_id = agent.id.split('-').next_back().unwrap_or("").to_string();
                // The mark, shown only while something is marked; unmarked
                // rows get the same width, so titles stay aligned.
                let mark = match (any_marked, self.marked.contains(&agent.id)) {
                    (false, _) => "",
                    (true, true) => "✓ ",
                    (true, false) => "  ",
                };
                // The fold arrow, on runs that have sub-agents. Leaves get the
                // same two columns of blank so every title still lines up.
                let arrow = if !tree.expandable {
                    "  "
                } else if tree.collapsed {
                    "▸ "
                } else {
                    "▾ "
                };
                // A fold has to say what it swallowed, or the run simply looks
                // like it has no workers.
                let hidden = if tree.collapsed {
                    format!(" +{}", tree.hidden)
                } else {
                    String::new()
                };
                // Only the title gives way: the id and the fold count are
                // what tell one row from another.
                let title_parts = [
                    mark.to_string(),
                    tree.prefix.clone(),
                    arrow.to_string(),
                    title_str,
                    format!(" #{}", short_id),
                    hidden,
                ];
                let title_colors = [C_ACCENT, C_DIM, C_ACCENT, C_WHITE, C_DIM, C_ACCENT];
                let title_spans: Vec<Span> = fit_parts(&title_parts, col_w[0], &[3])
                    .into_iter()
                    .zip(title_colors)
                    .map(|(text, color)| Span::styled(text, Style::default().fg(color)))
                    .collect();
                let title_cell = Cell::from(Line::from(title_spans));
                Row::new(vec![
                    title_cell,
                    Cell::from(truncate(&agent.blueprint_name, col_w[1])),
                    Cell::from(stage_str),
                    Cell::from(truncate(&status_str, col_w[3]))
                        .style(Style::default().fg(status_color)),
                    Cell::from(tok_str),
                    Cell::from(started_str).style(Style::default().fg(C_DIM)),
                ])
            })
            .collect();

        let empty_state_msg: Option<String> = if self.agents.is_empty() {
            Some(
                "  No agent runs yet. Press `n` to start one, or `a` to build an agent."
                    .to_string(),
            )
        } else if self.display_indices.is_empty() {
            Some(format!(
                "  No agent runs match \"{}\".",
                self.list_search_query
            ))
        } else {
            None
        };

        let mut list_title = if !self.list_search_query.is_empty() {
            format!(
                " Agent Runs  /{}/  {}/{} ",
                self.list_search_query,
                self.display_indices.len(),
                self.agents.len()
            )
        } else if self.list_search_mode {
            format!(" Agent Runs  /{}▌ ", self.list_search_query)
        } else {
            " Agent Runs ".to_string()
        };
        if any_marked {
            list_title = format!("{} {} marked ", list_title.trim_end(), self.marked.len());
        }

        // Register for wheel hit-testing and show which pane holds focus.
        self.pane_rects.push((PaneId::RunTable, area));
        let focused = self.main_focus == MainPane::RunList;
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if focused { C_BORDER_FOCUS } else { C_BORDER }))
            .title(Span::styled(
                list_title,
                Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
            ))
            .title_top(
                Line::from(Span::styled(
                    format!(" sort: {} ▾ ", self.sort_mode.label()),
                    Style::default().fg(C_DIM),
                ))
                .right_aligned(),
            );
        // The daemon link's chip, while there is something to say about it:
        // beside the sort chip, in the colour of how wrong things are.
        if let Some((chip, colour)) = self.daemon_link.chip() {
            block = block.title_top(
                Line::from(Span::styled(
                    chip,
                    Style::default().fg(colour).add_modifier(Modifier::BOLD),
                ))
                .right_aligned(),
            );
        }

        if let Some(msg) = empty_state_msg {
            let widget = Paragraph::new(Line::from(Span::styled(msg, Style::default().fg(C_DIM))))
                .block(block);
            frame.render_widget(widget, area);
            return;
        }

        let table = Table::new(rows, TABLE_COLUMNS)
            .header(header)
            .block(block)
            .row_highlight_style(
                Style::default()
                    .add_modifier(Modifier::REVERSED)
                    .fg(C_WHITE),
            );

        frame.render_stateful_widget(table, area, &mut self.table_state);
        self.register_run_row_clicks(area, any_marked);
    }

    /// Register a click target per visible run row, and a second one over its
    /// fold arrow.
    ///
    /// Runs after the table renders because that is when `TableState` knows
    /// how far it scrolled; deriving the offset ourselves would be a second
    /// implementation of the widget's own scrolling, free to disagree with it.
    fn register_run_row_clicks(&mut self, area: Rect, any_marked: bool) {
        // Inside the border, below the one-row header.
        let first_row_y = area.y.saturating_add(2);
        let visible = area.height.saturating_sub(3);
        let offset = self.table_state.offset();
        let title_x = area.x.saturating_add(1);
        // The tick column, present only while something is marked.
        let mark_w = if any_marked { 2u16 } else { 0 };
        for screen in 0..visible {
            let pos = offset + screen as usize;
            if pos >= self.display_indices.len() {
                break;
            }
            let y = first_row_y + screen;
            self.register_click(
                Rect::new(area.x, y, area.width, 1),
                ClickTarget::RunRow(pos),
            );
            let tree = self.tree_rows.get(pos).cloned().unwrap_or_default();
            if !tree.expandable {
                continue;
            }
            let prefix_w = UnicodeWidthStr::width(tree.prefix.as_str()) as u16;
            let arrow_x = title_x.saturating_add(mark_w).saturating_add(prefix_w);
            if arrow_x + 2 <= area.x + area.width {
                self.register_click(Rect::new(arrow_x, y, 2, 1), ClickTarget::RunToggle(pos));
            }
        }
    }

    pub(in crate::commands::dashboard) fn draw_log_panel(&mut self, frame: &mut Frame, area: Rect) {
        self.pane_rects.push((PaneId::LogPanel, area));
        // Clicking the panel is the mouse's Tab: it moves the keyboard here.
        self.register_click(area, ClickTarget::LogPanel);
        let viewport = area.height.saturating_sub(2) as usize;
        self.log_viewport = viewport;
        let window = self.log_scroll.window(self.log.len(), viewport);
        let scrolled_back = !self.log_scroll.is_tailing();

        let log_lines: Vec<Line> = self.log[window.clone()]
            .iter()
            .map(|entry| {
                Line::from(vec![
                    Span::styled(format!(" {} ", entry.timestamp), Style::default().fg(C_DIM)),
                    Span::styled(&entry.message, Style::default().fg(C_MUTED)),
                ])
            })
            .collect();

        let focused = self.main_focus == MainPane::LogPane;
        let title = if scrolled_back {
            format!(" Log  ↑{} (End resumes) ", self.log_scroll.offset_from_tail)
        } else {
            " Log ".to_string()
        };
        let log = Paragraph::new(log_lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(if focused { C_BORDER_FOCUS } else { C_BORDER }))
                .title(Span::styled(
                    title,
                    Style::default().fg(if scrolled_back { C_WARN } else { C_DIM }),
                )),
        );
        frame.render_widget(log, area);

        // A scrollbar whenever there is history beyond the window.
        if self.log.len() > viewport && viewport > 0 {
            let mut sb_state =
                ratatui::widgets::ScrollbarState::new(self.log.len().saturating_sub(viewport))
                    .position(window.start);
            frame.render_stateful_widget(
                ratatui::widgets::Scrollbar::new(
                    ratatui::widgets::ScrollbarOrientation::VerticalRight,
                ),
                area.inner(ratatui::layout::Margin {
                    vertical: 1,
                    horizontal: 0,
                }),
                &mut sb_state,
            );
        }
    }

    pub(in crate::commands::dashboard) fn draw_help_bar(&self, frame: &mut Frame, area: Rect) {
        use interaction::InteractionKind;

        let help = if self.pending_confirm.is_some() {
            Line::from(vec![
                Span::styled(
                    "[←/→]",
                    Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" choose  "),
                Span::styled(
                    "[Enter]",
                    Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" confirm  "),
                Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" cancel"),
            ])
        } else if self.main_focus == MainPane::LogPane && !self.detail_view && !self.mcp_screen {
            Line::from(vec![
                Span::styled(
                    "[↑↓]",
                    Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" scroll log  "),
                Span::styled("[PgUp/PgDn]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" screen  "),
                Span::styled("[End/G]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" newest  "),
                Span::styled("[Home/g]", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" oldest  "),
                Span::styled(
                    "[Tab/Esc]",
                    Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" back to list  "),
                Span::styled(
                    "[?]",
                    Style::default().fg(C_DIM).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" help  "),
                Span::styled(
                    "[q]",
                    Style::default().fg(C_DIM).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" quit"),
            ])
        } else if self.detail_view && self.input_mode {
            let kind = self
                .selected_agent()
                .and_then(|a| a.pending_request.as_ref())
                .map(|r| r.kind.clone());
            match kind {
                // The feedback box under an approval prompt: the response
                // box's keys, and Esc is back to the choices, not out.
                _ if self.deny_feedback_open => Line::from(vec![
                    Span::styled(
                        "[^S]",
                        Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" send with the deny  "),
                    Span::styled("[Enter]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" newline  "),
                    Span::styled("[Tab]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" Send button  "),
                    Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" back to the prompt"),
                ]),
                // The response box: Enter breaks the line and Ctrl+S sends
                // (Ctrl+Enter too, on a terminal that can tell it from
                // Enter), with the Send button for the mouse. An in-place
                // edit is the same box with a Save button, so it takes the
                // same bar.
                Some(InteractionKind::FreeText) | Some(InteractionKind::EditText) | None => {
                    Line::from(vec![
                        Span::styled(
                            "[^S]",
                            Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" send  "),
                        Span::styled("[Enter]", Style::default().add_modifier(Modifier::BOLD)),
                        Span::raw(" newline  "),
                        Span::styled("[Tab]", Style::default().add_modifier(Modifier::BOLD)),
                        Span::raw(" Send button  "),
                        Span::styled("[PgUp/PgDn]", Style::default().add_modifier(Modifier::BOLD)),
                        Span::raw(" scroll document  "),
                        Span::styled("[Esc]", Style::default().add_modifier(Modifier::BOLD)),
                        Span::raw(" cancel"),
                    ])
                }
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
                    Span::styled("[PgUp/PgDn]", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" scroll document  "),
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

        // On a narrow terminal the middle of the bar gives way; the first
        // keys and the last two (help and back/quit) stay.
        let help = crate::tui::widgets::footer::fit_help_line(help, area.width as usize, 2);
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
            Span::styled("[1-9]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" jump  "),
            Span::styled(
                "[g]",
                Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" graph  "),
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
                "[x]",
                Style::default().fg(C_ERROR).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" kill  "));
        }
        self.push_pause_resume_spans(&mut spans);
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

    /// Append a `[p] pause` or `[r] resume` hint matching what the selected
    /// agent's state actually allows (the same gates as the key handlers).
    fn push_pause_resume_spans(&self, spans: &mut Vec<Span<'static>>) {
        let Some(status) = self.selected_agent().map(|a| a.status.clone()) else {
            return;
        };
        if matches!(
            status,
            AgentDisplayStatus::Active | AgentDisplayStatus::Stale
        ) {
            spans.push(Span::styled(
                "[p]",
                Style::default().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" pause  "));
        } else if status == AgentDisplayStatus::Paused {
            spans.push(Span::styled(
                "[r]",
                Style::default().fg(C_WARN).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" resume  "));
        }
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
        ];
        // Only worth a slot on the bar when there is a tree to work: on a list
        // of plain runs the arrows have nothing to fold.
        if self.tree_rows.iter().any(|r| r.expandable) {
            spans.push(Span::styled(
                "[←/→]",
                Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" fold sub-agents  "));
        }
        spans.extend([
            Span::styled(
                "[n]",
                Style::default().fg(C_SUCCESS).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" new run  "),
            Span::styled(
                "[a]",
                Style::default().fg(C_SUCCESS).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" agents  "),
            Span::styled(
                "[Tab]",
                Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" log  "),
            Span::styled(
                "[/]",
                Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" filter  "),
            Span::styled("[s]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" sort  "),
            Span::styled("[d]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" delete  "),
            Span::styled("[space]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" mark  "),
            Span::styled("[m]", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" mcp  "),
        ]);
        if can_kill {
            spans.push(Span::styled(
                "[x]",
                Style::default().fg(C_ERROR).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(" kill  "));
        }
        self.push_pause_resume_spans(&mut spans);
        spans.push(Span::styled(
            "[?]",
            Style::default().fg(C_DIM).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" help  "));
        spans.push(Span::styled(
            "[q]",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" quit"));
        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::dashboard::test_support::{make_test_dashboard, style_at_text};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn make_test_agent(id: &str, status: AgentDisplayStatus) -> DashboardAgent {
        DashboardAgent {
            id: id.to_string(),
            blueprint_name: "test-agent".to_string(),
            stage: "main".to_string(),
            stage_index: 0,
            num_stages: 2,
            status,
            tokens_in: 100,
            tokens_out: 50,
            cached_tokens: 10,
            iteration: 3,
            broken_scripts: Vec::new(),
            waiting_prompt: None,
            wait_reason: None,
            pending_request: None,
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
            workdir: "/tmp/test".to_string(),
            task: "test task".to_string(),
            title: Some("My Test".to_string()),
            model: Some("claude-sonnet-4-20250514".to_string()),
            parent_id: None,
            started_at: chrono::Utc::now().timestamp() - 60,
            last_progress_at: None,
            runtime_secs: 0,
            clock_now: 0,
            graph: None,
            accepts_messages: true,
        }
    }

    /// While the daemon is unreachable the run list says so beside the sort
    /// chip, in the warning colour; a healthy link draws no chip at all.
    #[test]
    fn the_run_list_wears_the_daemon_chip_while_something_is_wrong() {
        let backend = TestBackend::new(140, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        terminal
            .draw(|f| dash.draw_agent_table(f, f.area()))
            .unwrap();
        assert!(
            !rendered_buffer(&terminal).contains("daemon"),
            "healthy: no chip"
        );

        dash.daemon_link.unreachable = true;
        terminal
            .draw(|f| dash.draw_agent_table(f, f.area()))
            .unwrap();
        let text = rendered_buffer(&terminal);
        assert!(text.contains("daemon unreachable, reconnecting"), "{text}");
        assert!(
            text.contains("sort:"),
            "the sort chip is still there: {text}"
        );
        assert_eq!(
            style_at_text(&terminal, "daemon unreachable").fg,
            Some(crate::tui::theme::C_WARN)
        );
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
        let buf = rendered_buffer(&terminal);
        assert!(!buf.contains("My Test"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(!buf.contains("My Test"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("My Test #1"), "{buf}");
        assert!(buf.contains("My Test #2"), "{buf}");
        assert!(buf.contains("My Test #3"), "{buf}");
    }

    #[test]
    fn draw_agent_table_non_run_state() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let agent = make_test_agent("run-ecs", AgentDisplayStatus::Active);
        dash.agents.push(agent);
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_agent_table(f, area);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("My Test #ecs"), "{buf}");
    }

    #[test]
    fn draw_agent_table_run_state_zero_tokens_shows_dash() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-zero-tok", AgentDisplayStatus::Active);
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("My Test #tok"), "{buf}");
        // The name of the case: zero tokens render as a dash, so neither
        // arrow glyph should be on the row at all.
        assert!(!buf.contains("\u{2191}"), "{buf}");
        assert!(!buf.contains("\u{2193}"), "{buf}");
    }

    #[test]
    fn draw_agent_table_title_none_falls_back_to_task() {
        // `title` is `None` before title generation completes (or when
        // disabled) - exercises the `.unwrap_or_else(|| truncate(&agent.task, 26))`
        // fallback closure, which every other test in this file never
        // reaches because `make_test_agent` always sets `title: Some(...)`.
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-no-title", AgentDisplayStatus::Active);
        agent.title = None;
        agent.task = "fallback task".to_string();
        dash.agents.push(agent);
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_agent_table(f, area);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("fallback task #title"), "{buf}");
    }

    #[test]
    fn draw_agent_table_single_stage_omits_stage_counter() {
        // `make_test_agent` defaults to `num_stages: 2`, always taking the
        // `agent.num_stages > 1` branch (stage name + "i/n" counter). A
        // single-stage agent takes the other arm (bare truncated stage
        // name, no counter) - never exercised elsewhere in this file.
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("My Test #stage"), "{buf}");
        assert!(!buf.contains("main 1/"), "{buf}");
    }

    #[test]
    fn draw_agent_table_non_run_state_zero_max_context() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let agent = make_test_agent("run-zero-ctx", AgentDisplayStatus::Active);
        dash.agents.push(agent);
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_agent_table(f, area);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("My Test #ctx"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("My Test #1"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(!buf.contains("Agent started"), "{buf}");
    }

    #[test]
    fn draw_log_panel_scrolled_back_shows_position_and_scrollbar() {
        let backend = TestBackend::new(120, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.log.clear();
        for i in 0..50 {
            dash.log.push(LogEntry {
                timestamp: "12:00:00".to_string(),
                message: format!("line {i}"),
            });
        }
        dash.main_focus = MainPane::LogPane;
        dash.log_scroll.scroll_up(7, 50, 10);
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_log_panel(f, area);
            })
            .unwrap();
        let text = rendered_buffer(&terminal);
        assert!(text.contains("↑7"), "{text}");
        assert!(text.contains("End resumes"), "{text}");
        assert!(!dash.pane_rects.is_empty(), "the pane registered its rect");
    }

    #[test]
    fn draw_help_bar_log_pane_focused() {
        let backend = TestBackend::new(200, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.main_focus = MainPane::LogPane;
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 200, 1);
                dash.draw_help_bar(f, area);
            })
            .unwrap();
        let text = rendered_buffer(&terminal);
        assert!(text.contains("scroll log"), "{text}");
        assert!(text.contains("back to list"), "{text}");
        // Every key the panel takes is on the bar, aliases included.
        assert!(text.contains("[PgUp/PgDn] screen"), "{text}");
        assert!(text.contains("[Home/g] oldest"), "{text}");
        assert!(text.contains("[End/G] newest"), "{text}");
        assert!(text.contains("[?] help"), "{text}");
    }

    #[test]
    fn draw_agent_table_unfocused_when_log_holds_focus() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.main_focus = MainPane::LogPane;
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_agent_table(f, area);
            })
            .unwrap();
        assert!(rendered_buffer(&terminal).contains("sort: started"));
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Agent started"), "{buf}");
        assert!(buf.contains("Stage changed to implement"), "{buf}");
    }

    fn rendered_buffer(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    /// The pause/resume hint tracks what the selected agent's state allows, in
    /// both the main list and the detail view: `[p]` for a pausable run, `[r]`
    /// for a paused one, neither for a finished one (or an empty list).
    #[test]
    fn help_bars_show_pause_or_resume_to_match_the_selected_agent() {
        for detail_view in [false, true] {
            for (status, expect_pause, expect_resume) in [
                (Some(AgentDisplayStatus::Active), true, false),
                (Some(AgentDisplayStatus::Paused), false, true),
                (Some(AgentDisplayStatus::Complete), false, false),
                (None, false, false),
            ] {
                let backend = TestBackend::new(200, 2);
                let mut terminal = Terminal::new(backend).unwrap();
                let mut dash = make_test_dashboard();
                if let Some(status) = &status {
                    dash.agents.push(make_test_agent("run-1", status.clone()));
                    dash.update_display_indices();
                }
                dash.detail_view = detail_view;
                terminal
                    .draw(|f| {
                        let area = Rect::new(0, 0, 200, 1);
                        dash.draw_help_bar(f, area);
                    })
                    .unwrap();
                let rendered = rendered_buffer(&terminal);
                assert_eq!(
                    rendered.contains("[p] pause"),
                    expect_pause,
                    "{status:?} detail={detail_view}"
                );
                assert_eq!(
                    rendered.contains("[r] resume"),
                    expect_resume,
                    "{status:?} detail={detail_view}"
                );
            }
        }
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
        let buf = rendered_buffer(&terminal);
        assert!(!buf.contains("back to list"), "{buf}");
        assert!(!buf.contains("Search: /"), "{buf}");
        // 120 columns is not enough for the whole bar: the middle gives way,
        // help and quit stay at the end.
        assert!(buf.contains("…  [?] help  [q] quit"), "{buf}");
        let backend = TestBackend::new(200, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 200, 1);
                dash.draw_help_bar(f, area);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("[m] mcp"), "the MCP screen has a key: {buf}");
        assert!(!buf.contains('…'), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("[Esc] back"), "{buf}");
        assert!(buf.contains("[g] graph"), "the graph has a key: {buf}");
        assert!(buf.contains("[1-9] jump"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        // Enter is a newline in the box; the bar must not say it sends.
        assert!(buf.contains("[^S] send"), "{buf}");
        assert!(buf.contains("[Enter] newline"), "{buf}");
        assert!(buf.contains("[Tab] Send button"), "{buf}");
        assert!(!buf.contains("[Enter] send"), "{buf}");
        assert!(buf.contains("[PgUp/PgDn] scroll document"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("select"), "{buf}");
    }

    /// With the feedback box open the bar names the box's keys, and says
    /// where Esc goes, because it does not go where it goes on the prompt.
    #[test]
    fn draw_help_bar_deny_feedback_box() {
        let backend = TestBackend::new(120, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-df", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(interaction::InteractionRequest::tool_approval(
            "ta1",
            "bash",
            serde_json::json!({"command": "ls"}),
            "main",
            &[],
        ));
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;
        dash.deny_feedback_open = true;
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.draw_help_bar(f, area);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("[^S] send with the deny"), "{buf}");
        assert!(buf.contains("[Esc] back to the prompt"), "{buf}");
        assert!(!buf.contains("select"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Search: /"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("/test/"), "{buf}");
        assert!(buf.contains("clear search"), "{buf}");
        assert!(!buf.contains("Search: /"), "{buf}");
    }

    #[test]
    fn draw_help_bar_confirm_delete() {
        let backend = TestBackend::new(120, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.request_delete();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 1);
                dash.draw_help_bar(f, area);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("confirm"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("Filter: /"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("/coder/"), "{buf}");
        assert!(buf.contains("refine"), "{buf}");
        assert!(!buf.contains("Filter: /"), "{buf}");
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
        assert!(text.contains("[x]"));
        assert!(text.contains("kill"));
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
        // 'm' was unified into 'i' - no separate [m] hint should remain.
        assert!(!text.contains("[m]"));
    }

    #[test]
    fn build_detail_help_bar_can_respond_shows_respond_label() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-resp", AgentDisplayStatus::Waiting);
        agent.waiting_prompt = Some("prompt".to_string());
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::free_text(
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

    // ── Why a run is waiting ──────────────────────────────────────────────

    /// A parked run says what it is parked on, so WAITING stops meaning four
    /// different things.
    #[test]
    fn a_waiting_row_names_what_it_is_waiting_on() {
        use leviath_core::run_meta::WaitReason;

        for (reason, expected) in [
            (WaitReason::ToolApproval, "tool approval"),
            (WaitReason::FanOutWorkers { outstanding: 3 }, "workers(3)"),
            (WaitReason::Children { outstanding: 2 }, "children(2)"),
        ] {
            let backend = TestBackend::new(120, 40);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut dash = make_test_dashboard();
            let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
            agent.wait_reason = Some(reason);
            dash.agents.push(agent);
            dash.update_display_indices();
            terminal
                .draw(|f| {
                    let area = f.area();
                    dash.draw_agent_table(f, area);
                })
                .unwrap();
            let buf = rendered_buffer(&terminal);
            assert!(buf.contains(expected), "expected {expected}: {buf}");
            // The reason replaces the bare word: this column is too narrow to
            // carry both, and the word is the half that says nothing.
            assert!(!buf.contains("WAITING"), "{buf}");
        }
    }

    /// A run parked until the machine is fixed says what it needs, rather than
    /// showing a bare PAUSED that reads as somebody's own decision.
    #[test]
    fn a_parked_row_names_what_the_machine_is_missing() {
        use leviath_core::run_meta::{SetupBlocker, WaitReason};

        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Paused);
        agent.wait_reason = Some(WaitReason::NeedsSetup {
            blocker: SetupBlocker::ProviderMissing,
            remedy: "add it to config.toml".to_string(),
        });
        dash.agents.push(agent);
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_agent_table(f, area);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("needs provider"), "{buf}");
        assert!(
            !buf.contains("PAUSED"),
            "the reason replaces the word: {buf}"
        );
    }

    /// A run somebody paused themselves has no reason, and reads as it always
    /// did.
    #[test]
    fn a_row_paused_by_a_person_reads_as_it_always_did() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Paused));
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_agent_table(f, area);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("PAUSED"), "{buf}");
    }

    /// A run whose `meta.json` predates the field renders exactly as it did
    /// before, rather than claiming a reason nobody recorded.
    #[test]
    fn a_waiting_row_without_a_reason_reads_as_it_always_did() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.wait_reason = None;
        dash.agents.push(agent);
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_agent_table(f, area);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("WAITING"), "{buf}");
        assert!(!buf.contains("workers("), "{buf}");
    }

    // ── Marks for group kill / delete ─────────────────────────────────────

    #[test]
    fn marked_rows_show_a_check_and_the_title_counts_them() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.agents
            .push(make_test_agent("run-2", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.marked.insert("run-1".to_string());
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_agent_table(f, area);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        // The two columns between the tick and the title are the fold-arrow
        // gutter, blank on a run with no sub-agents so titles stay aligned.
        assert!(buf.contains("✓   My Test #1"), "{buf}");
        assert!(!buf.contains("✓   My Test #2"), "{buf}");
        assert!(buf.contains("My Test #2"), "the unmarked row still renders");
        assert!(buf.contains("1 marked"), "{buf}");
    }

    // ── The sub-agent fold ────────────────────────────────────────────────

    /// A parent with one worker, the shape the run list draws as a tree.
    fn dash_with_a_worker() -> crate::commands::dashboard::state::Dashboard {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("parent", AgentDisplayStatus::Active));
        let mut child = make_test_agent("worker", AgentDisplayStatus::Active);
        child.parent_id = Some("parent".to_string());
        dash.agents.push(child);
        dash.update_display_indices();
        dash
    }

    /// The arrow says which way the fold goes, and a folded row says how many
    /// runs are behind it.
    #[test]
    fn the_fold_arrow_and_its_hidden_count_are_drawn() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = dash_with_a_worker();
        terminal
            .draw(|f| dash.draw_agent_table(f, f.area()))
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("▾ My Test"), "an unfolded parent: {buf}");
        assert!(!buf.contains("▸ "), "nothing is folded yet: {buf}");

        dash.collapsed_runs.insert("parent".to_string());
        dash.update_display_indices();
        terminal
            .draw(|f| dash.draw_agent_table(f, f.area()))
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("▸ My Test"), "a folded parent: {buf}");
        assert!(buf.contains("+1"), "and what it hides: {buf}");
    }

    /// The arrow's rect is registered where the arrow was actually drawn, and
    /// only for rows that have one.
    #[test]
    fn only_a_foldable_row_registers_a_fold_arrow() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = dash_with_a_worker();
        terminal
            .draw(|f| dash.draw_agent_table(f, f.area()))
            .unwrap();
        let toggles: Vec<_> = dash
            .click_targets
            .iter()
            .filter(|(_, t)| matches!(t, ClickTarget::RunToggle(_)))
            .collect();
        assert_eq!(toggles.len(), 1, "the parent only, not the worker");
        assert_eq!(toggles[0].1, ClickTarget::RunToggle(0));
        let rows = dash
            .click_targets
            .iter()
            .filter(|(_, t)| matches!(t, ClickTarget::RunRow(_)))
            .count();
        assert_eq!(rows, 2, "both rows are clickable");
    }

    /// A table too narrow to hold the arrow registers the row but no arrow,
    /// rather than a rect hanging off the right edge.
    #[test]
    fn a_table_narrower_than_the_arrow_registers_no_arrow() {
        let backend = TestBackend::new(2, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = dash_with_a_worker();
        terminal
            .draw(|f| dash.draw_agent_table(f, f.area()))
            .unwrap();
        assert!(
            !dash
                .click_targets
                .iter()
                .any(|(_, t)| matches!(t, ClickTarget::RunToggle(_))),
            "no room for an arrow to click"
        );
    }

    /// The fold hint earns its place on the bar only when there is a tree.
    #[test]
    fn the_fold_hint_appears_only_with_sub_agents() {
        let backend = TestBackend::new(200, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("solo", AgentDisplayStatus::Active));
        dash.update_display_indices();
        terminal
            .draw(|f| dash.draw_help_bar(f, Rect::new(0, 0, 200, 1)))
            .unwrap();
        assert!(
            !rendered_buffer(&terminal).contains("fold sub-agents"),
            "no tree, no hint"
        );

        let dash = dash_with_a_worker();
        terminal
            .draw(|f| dash.draw_help_bar(f, Rect::new(0, 0, 200, 1)))
            .unwrap();
        assert!(
            rendered_buffer(&terminal).contains("fold sub-agents"),
            "the hint shows once a run has workers"
        );
    }

    #[test]
    fn table_without_marks_shows_no_check_or_count() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_agent_table(f, area);
            })
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(!buf.contains('✓'), "{buf}");
        assert!(!buf.contains("marked"), "{buf}");
    }

    #[test]
    fn main_list_help_bar_offers_the_mark_key() {
        let dash = make_test_dashboard();
        let line = dash.build_main_list_help_bar();
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("[space] mark"), "{text}");
    }

    // ── Cells cut to their column ──────────────────────────────────────────

    fn table_at(width: u16, dash: &mut Dashboard) -> String {
        let backend = TestBackend::new(width, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                dash.draw_agent_table(f, area);
            })
            .unwrap();
        rendered_buffer(&terminal)
    }

    /// The widths the table draws with sum to the inner width less the five
    /// gaps, and a zero-width area does not underflow.
    #[test]
    fn table_column_widths_follow_the_layout() {
        let widths = table_column_widths(Rect::new(0, 0, 120, 10));
        assert_eq!(widths.len(), 6);
        assert_eq!(widths.iter().sum::<usize>(), 118 - 5);
        assert_eq!(
            table_column_widths(Rect::new(0, 0, 0, 0))
                .iter()
                .sum::<usize>(),
            0
        );
    }

    /// A long title is whole when its column is wide enough and cut with an
    /// ellipsis, keeping its id, when it is not. Before, it was always cut to
    /// 26 characters, and a title longer than the column was clipped silently.
    #[test]
    fn draw_agent_table_cuts_the_title_to_its_column_and_keeps_the_id() {
        let title = "Add adversarial and cross-company risk analysis";
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-abc-zz9", AgentDisplayStatus::Active);
        agent.title = Some(title.to_string());
        dash.agents.push(agent);
        dash.update_display_indices();

        let wide = table_at(300, &mut dash);
        assert!(wide.contains(&format!("{title} #zz9")), "{wide}");

        let narrow = table_at(100, &mut dash);
        assert!(!narrow.contains(title), "{narrow}");
        assert!(narrow.contains("… #zz9"), "{narrow}");
    }

    /// The blueprint, stage, and status cells are cut to their columns too,
    /// and the stage counter outlives the stage name.
    #[test]
    fn draw_agent_table_cuts_the_other_text_cells_to_their_columns() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-cells", AgentDisplayStatus::Active);
        agent.blueprint_name = "a-very-long-blueprint-name-indeed".to_string();
        agent.stage = "gather-and-cross-check-sources".to_string();
        agent.num_stages = 3;
        agent.stage_index = 1;
        agent.status = AgentDisplayStatus::Error(
            "provider returned 502 after three attempts and the breaker opened".to_string(),
        );
        dash.agents.push(agent);
        dash.update_display_indices();

        let buf = table_at(100, &mut dash);
        assert!(!buf.contains("a-very-long-blueprint-name-indeed"), "{buf}");
        assert!(buf.contains("a-very-l"), "{buf}");
        assert!(!buf.contains("gather-and-cross-check-sources"), "{buf}");
        assert!(buf.contains("… 2/3"), "{buf}");
        assert!(!buf.contains("breaker opened"), "{buf}");
        assert!(buf.contains("ERROR: "), "{buf}");

        let wide = table_at(500, &mut dash);
        assert!(wide.contains("a-very-long-blueprint-name-indeed"), "{wide}");
        assert!(
            wide.contains("gather-and-cross-check-sources 2/3"),
            "{wide}"
        );
        assert!(wide.contains("breaker opened"), "{wide}");
    }

    /// An in-place edit is the response box with a Save button, and takes the
    /// same keys: the bar must not offer it the choice list's "[Enter]
    /// confirm" while Enter breaks the line.
    #[test]
    fn draw_help_bar_input_mode_edit_text() {
        let backend = TestBackend::new(120, 2);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-edit", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(interaction::InteractionRequest {
            id: "req-e".to_string(),
            kind: interaction::InteractionKind::EditText,
            prompt: "Edit this".to_string(),
            options: vec![],
            tool_name: None,
            tool_arguments: None,
            required: true,
            stage_name: "main".to_string(),
            body: Some("draft".to_string()),
            body_format: interaction::BodyFormat::Plain,
        });
        agent.waiting_prompt = Some("Edit this".to_string());
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("[^S] send"), "{buf}");
        assert!(buf.contains("[Enter] newline"), "{buf}");
        assert!(buf.contains("[Tab] Send button"), "{buf}");
        assert!(!buf.contains("[Enter] confirm"), "{buf}");
    }
}
