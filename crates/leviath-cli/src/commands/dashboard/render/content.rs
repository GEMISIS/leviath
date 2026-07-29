//! Content pane rendering: output, logs, context view, search highlighting.

use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};

use crate::commands::dashboard::helpers::format_tokens;
use crate::commands::dashboard::state::Dashboard;
use crate::commands::dashboard::theme::*;
use crate::commands::dashboard::types::*;
use crate::runstate;

/// Replace a leading `home` prefix in `raw` with `~`. Split out so both the
/// shortened and the raw (path-is-outside-home) branches are unit-testable on
/// every platform: reaching the raw branch through the real render path needs a
/// runs dir outside `$HOME`, which isn't portable (`std::env::temp_dir()` lives
/// *under* the home directory on Windows, so it only ever hits the shortened
/// branch there).
#[expect(
    clippy::string_slice,
    reason = "the `starts_with(home)` guard makes `home.len()` the end of a matched prefix, which \
              is a char boundary"
)]
fn shorten_home_path(raw: String, home: &str) -> String {
    if !home.is_empty() && raw.starts_with(home) {
        format!("~{}", &raw[home.len()..])
    } else {
        raw
    }
}

impl Dashboard {
    pub(in crate::commands::dashboard) fn render_context_bar(
        &self,
        frame: &mut Frame,
        ctx_area: Rect,
        agent: &DashboardAgent,
    ) {
        // When browsing archived history, show that point; else the live window.
        let snap_opt = self
            .browsed_context_point()
            .map(|p| p.context.clone())
            .or_else(|| runstate::read_stage_context(&agent.id, self.selected_stage))
            .or_else(|| agent.context_snapshot.clone());

        // The card title shows the browsed history position (or a plain " ctx ").
        let title = match self.context_history_idx {
            Some(i) => format!(" ctx {}/{} ", i + 1, self.context_history.len()),
            None => " ctx ".to_string(),
        };

        // Constrain context card to at most 60 cols, left-aligned
        let card_w = ctx_area.width.min(64);
        let card_area = Rect {
            width: card_w,
            ..ctx_area
        };

        if let Some(snap) = snap_opt {
            let total_pct = (snap.total_tokens * 100)
                .checked_div(snap.max_tokens)
                .unwrap_or(0)
                .min(100);
            let bar_color = if total_pct >= 90 {
                C_ERROR
            } else if total_pct >= 70 {
                C_WARN
            } else {
                C_SUCCESS
            };

            let inner_w = (card_w as usize).saturating_sub(4).max(8);
            let bar_w = inner_w.min(32);
            let filled = bar_w * total_pct / 100;
            let bar = format!("{}{}", "█".repeat(filled), "░".repeat(bar_w - filled));

            let regions_str: String = snap
                .regions
                .iter()
                .take(6)
                .map(|r| match r.kind.as_str() {
                    "pinned" => "P",
                    "sliding" => "S",
                    "compacting" | "history" => "H",
                    _ => "·",
                })
                .collect::<Vec<_>>()
                .join(" ");

            let bar_line = Line::from(vec![
                Span::styled(bar, Style::default().fg(bar_color)),
                Span::styled(
                    format!("  {}%", total_pct),
                    Style::default().fg(C_DIM).add_modifier(Modifier::BOLD),
                ),
            ]);
            let info_line = Line::from(vec![
                Span::styled(
                    format!(
                        "{} / {} tokens",
                        format_tokens(snap.total_tokens),
                        format_tokens(snap.max_tokens)
                    ),
                    Style::default().fg(C_MUTED),
                ),
                Span::styled(
                    if regions_str.is_empty() {
                        String::new()
                    } else {
                        format!("   [{}]", regions_str)
                    },
                    Style::default().fg(C_DIM),
                ),
            ]);

            frame.render_widget(
                Paragraph::new(vec![bar_line, info_line]).block(
                    Block::default()
                        .title(Span::styled(title.clone(), Style::default().fg(C_DIM)))
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(C_BORDER)),
                ),
                card_area,
            );
        } else {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "no context snapshot yet",
                    Style::default().fg(C_DIM),
                )))
                .block(
                    Block::default()
                        .title(Span::styled(title.clone(), Style::default().fg(C_DIM)))
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(C_BORDER)),
                ),
                card_area,
            );
        }
    }

    pub(in crate::commands::dashboard) fn render_content_pane(
        &mut self,
        frame: &mut Frame,
        content_area: Rect,
        agent: &DashboardAgent,
        _area_width: u16,
    ) {
        // Editing a document (an `EditText` interaction) takes over the content
        // pane: the editable textarea is rendered here, over the current text,
        // instead of the read-only stage output — so the user revises the plan
        // in place rather than in the bottom input bar.
        if self.editing_document() {
            self.input_textarea.set_block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(C_SUCCESS))
                    .title(Span::styled(
                        " ✎ Editing this document — your changes replace it  ·  [Enter] save  \
                         [Alt+↵] newline  [Esc] cancel ",
                        Style::default().fg(C_SUCCESS).add_modifier(Modifier::BOLD),
                    )),
            );
            self.input_textarea.set_style(Style::default().fg(C_WHITE));
            self.input_textarea.set_cursor_style(
                Style::default()
                    .fg(ratatui::style::Color::Black)
                    .bg(C_ACCENT),
            );
            frame.render_widget(&self.input_textarea, content_area);
            return;
        }

        // Read-only stage content accepts mouse selection; the rect excludes
        // the borders (and with them the scrollbar track, which sits on the
        // right border column). The editor branch above deliberately does not
        // register: tui-textarea owns those cells.
        self.selection_regions.push(content_area.inner(Margin {
            vertical: 1,
            horizontal: 1,
        }));

        let inner_h = content_area.height.saturating_sub(2) as usize;
        let render_width = content_area.width.saturating_sub(2);
        let is_context = self.stage_content_mode == StageContentMode::Context;
        let is_output = self.stage_content_mode == StageContentMode::Output;

        // Build content lines
        let all_lines: Vec<Line> = if is_context {
            self.build_context_lines(agent, render_width)
        } else {
            self.build_output_lines(agent, is_output, render_width)
        };

        // ── Error / Cancelled banner ─────────────────────────────────────
        let mut all_lines = all_lines;
        match &agent.status {
            AgentDisplayStatus::Error(msg) if !msg.is_empty() => {
                all_lines.push(Line::from(vec![
                    Span::styled(
                        " ✗ Error  ",
                        Style::default()
                            .fg(Color::Black)
                            .bg(C_ERROR)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!(" {}", msg), Style::default().fg(C_ERROR)),
                ]));
            }
            AgentDisplayStatus::Error(_) => {
                all_lines.push(Line::from(Span::styled(
                    " ✗ Agent terminated with an error.",
                    Style::default().fg(C_ERROR),
                )));
            }
            AgentDisplayStatus::Cancelled => {
                all_lines.push(Line::from(Span::styled(
                    " ⊘ Run was cancelled.",
                    Style::default().fg(C_DIM),
                )));
            }
            _ => {}
        }

        let total = all_lines.len();

        // ── Search: compute match indices + navigate ──────────────────────
        let query_lc = self.search_query.to_lowercase();
        let match_indices: Vec<usize> = if query_lc.is_empty() {
            Vec::new()
        } else {
            all_lines
                .iter()
                .enumerate()
                .filter_map(|(i, line)| {
                    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                    if text.to_lowercase().contains(&query_lc) {
                        Some(i)
                    } else {
                        None
                    }
                })
                .collect()
        };

        // Clamp search_match_idx and jump to current match
        if !match_indices.is_empty() {
            self.search_match_idx = self.search_match_idx.min(match_indices.len() - 1);
            let match_line = match_indices[self.search_match_idx];
            let center_scroll = total.saturating_sub(match_line + inner_h / 2);
            self.detail_scroll = center_scroll;
        }

        let max_scroll = total.saturating_sub(inner_h);
        if self.detail_scroll > max_scroll {
            self.detail_scroll = max_scroll;
        }
        let start = total.saturating_sub(inner_h + self.detail_scroll);
        let end = (start + inner_h).min(total);

        let visible: Vec<Line> = if total == 0 {
            let stage_name = agent
                .stages
                .get(self.selected_stage)
                .map(|s| s.name.as_str())
                .unwrap_or("this stage");
            vec![Line::from(Span::styled(
                format!(
                    " No {} yet for {}.",
                    if is_output { "output" } else { "logs" },
                    stage_name
                ),
                Style::default().fg(C_DIM),
            ))]
        } else {
            let current_match_line = match_indices.get(self.search_match_idx).copied();
            all_lines[start..end]
                .iter()
                .enumerate()
                .map(|(rel_idx, line)| {
                    let abs_idx = start + rel_idx;
                    let is_current_match = current_match_line == Some(abs_idx);
                    let is_any_match = !query_lc.is_empty() && match_indices.contains(&abs_idx);
                    if is_current_match {
                        Line::from(
                            line.spans
                                .iter()
                                .map(|s| {
                                    Span::styled(
                                        s.content.clone(),
                                        Style::default()
                                            .fg(Color::Black)
                                            .bg(Color::Yellow)
                                            .add_modifier(Modifier::BOLD),
                                    )
                                })
                                .collect::<Vec<_>>(),
                        )
                    } else if is_any_match {
                        Line::from(
                            line.spans
                                .iter()
                                .map(|s| {
                                    Span::styled(
                                        s.content.clone(),
                                        Style::default().fg(C_WHITE).bg(Color::Rgb(80, 60, 0)),
                                    )
                                })
                                .collect::<Vec<_>>(),
                        )
                    } else {
                        line.clone()
                    }
                })
                .collect()
        };

        // Tool count badge for logs tab
        let tool_count = if self.stage_content_mode == StageContentMode::Logs {
            let raw = runstate::tail_stage_log(&agent.id, self.selected_stage, 131_072);
            let tc = raw.lines().filter(|l| l.starts_with("[tool]")).count();
            if tc > 0 {
                format!(" · {} tools", tc)
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        // Search indicator in the title
        let search_indicator = if !query_lc.is_empty() {
            if match_indices.is_empty() {
                format!(" 🔍/{}/  0 matches", self.search_query)
            } else {
                format!(
                    " /{}/  {}/{}",
                    self.search_query,
                    self.search_match_idx + 1,
                    match_indices.len()
                )
            }
        } else if self.search_mode {
            format!(" /{}▌", self.search_query)
        } else {
            String::new()
        };

        let mode_label = match self.stage_content_mode {
            StageContentMode::Output => format!(
                " Output  [l] logs  [c] ctx{}{} ",
                tool_count, search_indicator
            ),
            StageContentMode::Logs => format!(
                " Logs  [o] output  [c] ctx{}{} ",
                tool_count, search_indicator
            ),
            StageContentMode::Context => {
                format!(" Context Window  [o] output  [l] logs{} ", search_indicator)
            }
        };
        let scroll_info = if total > inner_h {
            let pct = 100
                - (self.detail_scroll.min(max_scroll) * 100)
                    .checked_div(max_scroll)
                    .unwrap_or(0);
            format!(" {}% ({}/{}) ", pct, end, total)
        } else {
            String::new()
        };

        // Bottom-left file path hint
        let file_path_hint = {
            let file_name = match self.stage_content_mode {
                StageContentMode::Output => "output.log",
                StageContentMode::Logs => "logs.log",
                StageContentMode::Context => "context.json",
            };
            let raw = runstate::stage_dir(&agent.id, self.selected_stage)
                .join(file_name)
                .to_string_lossy()
                .to_string();
            let home = dirs::home_dir()
                .map(|h| h.to_string_lossy().to_string())
                .unwrap_or_default();
            let shortened = shorten_home_path(raw, &home);
            format!(" {} ", shortened)
        };

        let content_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(C_BORDER_FOCUS))
            .title(Span::styled(mode_label, Style::default().fg(C_ACCENT)))
            .title_bottom(
                Line::from(Span::styled(file_path_hint, Style::default().fg(C_DIM))).left_aligned(),
            )
            .title_bottom(Span::styled(scroll_info, Style::default().fg(C_DIM)));

        let content_widget = Paragraph::new(visible)
            .block(content_block)
            .wrap(Wrap { trim: false });
        frame.render_widget(content_widget, content_area);

        // Scrollbar
        if total > inner_h {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓"));
            let mut sb_state = ScrollbarState::new(max_scroll)
                .position(max_scroll.saturating_sub(self.detail_scroll));
            frame.render_stateful_widget(
                scrollbar,
                content_area.inner(Margin {
                    vertical: 1,
                    horizontal: 0,
                }),
                &mut sb_state,
            );
        }
    }

    fn build_context_lines(&self, agent: &DashboardAgent, render_width: u16) -> Vec<Line<'static>> {
        // When browsing the run's archived context history, show that point's
        // window; otherwise the live current window for the selected stage.
        let snap_opt = self
            .browsed_context_point()
            .map(|p| p.context.clone())
            .or_else(|| runstate::read_stage_context(&agent.id, self.selected_stage))
            .or_else(|| agent.context_snapshot.clone());
        if let Some(snap) = snap_opt {
            let mut lines: Vec<Line> = Vec::new();

            // ── Graph transition details ──
            if let Some(ref graph) = agent.graph_info {
                let sel_name = agent
                    .stages
                    .get(self.selected_stage)
                    .map(|s| s.name.as_str())
                    .or_else(|| {
                        graph
                            .stage_names
                            .get(self.selected_stage)
                            .map(|s| s.as_str())
                    })
                    .unwrap_or(&agent.stage);

                lines.push(Line::from(vec![
                    Span::styled(
                        "▌ ",
                        Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("Stage: {}", sel_name),
                        Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                    ),
                ]));

                let vc = agent.stages.iter().filter(|s| s.name == sel_name).count();
                if vc > 0 {
                    lines.push(Line::from(Span::styled(
                        format!("  Visited {} time{}", vc, if vc != 1 { "s" } else { "" }),
                        Style::default().fg(C_MUTED),
                    )));
                }

                // Outgoing transitions
                if let Some(edges) = graph.edges.get(sel_name) {
                    if edges.is_empty() {
                        lines.push(Line::from(Span::styled(
                            "  Transitions: (terminal — no outgoing edges)",
                            Style::default().fg(C_DIM),
                        )));
                    } else {
                        lines.push(Line::from(Span::styled(
                            "  Transitions:",
                            Style::default().fg(C_MUTED),
                        )));
                        for edge in edges {
                            let cond_part = if edge.condition != "always" {
                                format!(" [{}]", edge.condition)
                            } else {
                                String::new()
                            };
                            let hint_part = edge
                                .hint
                                .as_deref()
                                .map(|h| format!(" — {}", h))
                                .unwrap_or_default();
                            lines.push(Line::from(vec![
                                Span::styled(
                                    format!("    → {}", edge.target),
                                    Style::default().fg(C_ACCENT),
                                ),
                                Span::styled(cond_part, Style::default().fg(C_WARN)),
                                Span::styled(hint_part, Style::default().fg(C_DIM)),
                            ]));
                        }
                    }
                } else {
                    lines.push(Line::from(Span::styled(
                        "  Transitions: (linear — no graph edges)",
                        Style::default().fg(C_DIM),
                    )));
                }

                // Incoming transitions
                let incoming: Vec<(&str, &crate::commands::dashboard::graph::GraphEdge)> = graph
                    .edges
                    .iter()
                    .flat_map(|(src, edges)| {
                        edges
                            .iter()
                            .filter(|e| e.target == sel_name)
                            .map(move |e| (src.as_str(), e))
                    })
                    .collect();
                if !incoming.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "  Incoming from:",
                        Style::default().fg(C_MUTED),
                    )));
                    for (src, edge) in &incoming {
                        let transform_part = format!(" [transform: {}]", edge.transform);
                        lines.push(Line::from(vec![
                            Span::styled(format!("    ← {}", src), Style::default().fg(C_SUCCESS)),
                            Span::styled(transform_part, Style::default().fg(C_DIM)),
                        ]));
                    }
                }

                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "─".repeat(32),
                    Style::default().fg(C_DIM),
                )));
                lines.push(Line::from(""));
            }

            // Overall usage header
            let total_pct = (snap.total_tokens * 100)
                .checked_div(snap.max_tokens)
                .unwrap_or(0)
                .min(100);
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {} regions  ", snap.regions.len()),
                    Style::default().fg(C_DIM),
                ),
                Span::styled(
                    format!(
                        "{}/{} tokens total  {}%",
                        format_tokens(snap.total_tokens),
                        format_tokens(snap.max_tokens),
                        total_pct
                    ),
                    Style::default().fg(C_MUTED),
                ),
            ]));

            // Detect old runs
            let has_tokens = snap.regions.iter().any(|r| r.current_tokens > 0);
            let has_entries = snap.regions.iter().any(|r| !r.entries.is_empty());
            if has_tokens && !has_entries {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    " ℹ  This run predates context content capture.",
                    Style::default().fg(C_WARN),
                )));
                lines.push(Line::from(Span::styled(
                    "    Token counts are shown but entry content is unavailable.",
                    Style::default().fg(C_DIM),
                )));
                lines.push(Line::from(Span::styled(
                    "    Re-run the agent to see full context details.",
                    Style::default().fg(C_DIM),
                )));
                lines.push(Line::from(""));
            }

            lines.push(Line::from(""));
            for region in &snap.regions {
                let pct = (region.current_tokens * 100)
                    .checked_div(region.max_tokens)
                    .unwrap_or(0)
                    .min(100);
                let bar_w = 16usize;
                let filled = bar_w * pct / 100;
                let bar = format!("{}{}", "█".repeat(filled), "░".repeat(bar_w - filled));
                let bar_color = if pct >= 90 {
                    C_ERROR
                } else if pct >= 70 {
                    C_WARN
                } else if pct > 0 {
                    C_SUCCESS
                } else {
                    C_DIM
                };
                let kind_color = match region.kind.as_str() {
                    "pinned" => C_ACCENT,
                    "sliding" => C_SUCCESS,
                    "compacting" | "history" => C_WARN,
                    "temporary" | "clearable" => C_MUTED,
                    _ => C_DIM,
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        "▌ ",
                        Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{:<16}", region.name),
                        Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{:<12}", region.kind),
                        Style::default().fg(kind_color),
                    ),
                    Span::styled(bar, Style::default().fg(bar_color)),
                    Span::styled(
                        format!(
                            "  {}/{}",
                            format_tokens(region.current_tokens),
                            format_tokens(region.max_tokens)
                        ),
                        Style::default().fg(C_DIM),
                    ),
                ]));
                if region.entries.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "  (empty)",
                        Style::default().fg(C_DIM),
                    )));
                } else {
                    for (idx, entry) in region.entries.iter().enumerate() {
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("  ┄ entry {}  ", idx + 1),
                                Style::default().fg(C_DIM),
                            ),
                            Span::styled(
                                format!("{} tokens", entry.tokens),
                                Style::default().fg(C_DIM),
                            ),
                        ]));
                        let rendered = crate::render::markdown_to_text(
                            &entry.content,
                            render_width.saturating_sub(2),
                        );
                        for mut l in rendered.lines {
                            l.spans.insert(0, Span::raw("  "));
                            lines.push(l);
                        }
                    }
                }
                lines.push(Line::from(""));
            }
            lines
        } else {
            vec![Line::from(Span::styled(
                " no context snapshot available for this stage",
                Style::default().fg(C_DIM),
            ))]
        }
    }

    #[expect(
        clippy::string_slice,
        reason = "`prefix_end` is only non-zero on a `starts_with` branch, where it is the length \
                  of the ASCII tag that just matched — a char boundary"
    )]
    fn build_output_lines(
        &self,
        agent: &DashboardAgent,
        is_output: bool,
        render_width: u16,
    ) -> Vec<Line<'static>> {
        let content = if is_output {
            // When a document is up for review (a pending interaction's body,
            // e.g. the plan being approved), show just that current instance —
            // not the full accumulated output history. `[l]` still shows logs.
            match self.reviewing_body() {
                Some(body) => body,
                None => runstate::tail_stage_output(&agent.id, self.selected_stage, 131_072),
            }
        } else {
            runstate::tail_stage_log(&agent.id, self.selected_stage, 131_072)
        };

        if is_output && !content.is_empty() {
            crate::render::markdown_to_text(&content, render_width).lines
        } else if !is_output {
            content
                .lines()
                .map(|l| {
                    let (color, prefix_end) = if l.starts_with("[tool]") {
                        (C_ACCENT, 6)
                    } else if l.starts_with("[error]") {
                        (C_ERROR, 7)
                    } else if l.starts_with("[denied]") {
                        (C_WARN, 8)
                    } else if l.starts_with("---") || l.starts_with("[All") {
                        (C_DIM, 0)
                    } else {
                        (C_MUTED, 0)
                    };
                    if prefix_end > 0 && l.len() > prefix_end {
                        Line::from(vec![
                            Span::styled(
                                format!(" {}", &l[..prefix_end]),
                                Style::default().fg(color).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(l[prefix_end..].to_string(), Style::default().fg(C_MUTED)),
                        ])
                    } else {
                        Line::from(Span::styled(format!(" {}", l), Style::default().fg(color)))
                    }
                })
                .collect()
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::dashboard::test_support::make_test_dashboard;
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

    fn make_context_snapshot(total: usize, max: usize) -> runstate::ContextSnapshot {
        runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: total,
            max_tokens: max,
            regions: vec![runstate::RegionSnapshot {
                name: "system".to_string(),
                kind: "pinned".to_string(),
                current_tokens: total / 2,
                max_tokens: max / 2,
                entries: vec![],
            }],
        }
    }

    #[test]
    fn render_context_bar_with_snapshot() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-ctx", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(make_context_snapshot(4000, 8000));
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 5);
                dash.render_context_bar(f, area, &agent);
            })
            .unwrap();
    }

    #[test]
    fn render_context_bar_without_snapshot() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-noctx", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 5);
                dash.render_context_bar(f, area, &agent);
            })
            .unwrap();
    }

    #[test]
    fn render_context_bar_high_fill() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-high", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(make_context_snapshot(7500, 8000)); // 93% = red
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 5);
                dash.render_context_bar(f, area, &agent);
            })
            .unwrap();
    }

    #[test]
    fn render_context_bar_medium_fill() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-med", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(make_context_snapshot(6000, 8000)); // 75% = yellow
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 5);
                dash.render_context_bar(f, area, &agent);
            })
            .unwrap();
    }

    #[test]
    fn render_context_bar_multiple_regions() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-multi", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 5000,
            max_tokens: 8000,
            regions: vec![
                runstate::RegionSnapshot {
                    name: "system".to_string(),
                    kind: "pinned".to_string(),
                    current_tokens: 1000,
                    max_tokens: 2000,
                    entries: vec![],
                },
                runstate::RegionSnapshot {
                    name: "history".to_string(),
                    kind: "sliding".to_string(),
                    current_tokens: 3000,
                    max_tokens: 4000,
                    entries: vec![],
                },
                runstate::RegionSnapshot {
                    name: "context".to_string(),
                    kind: "compacting".to_string(),
                    current_tokens: 1000,
                    max_tokens: 2000,
                    entries: vec![],
                },
            ],
        });
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 5);
                dash.render_context_bar(f, area, &agent);
            })
            .unwrap();
    }

    #[test]
    fn render_content_pane_output_mode() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        let agent = make_test_agent("run-out", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
    }

    #[test]
    fn render_content_pane_shows_pending_review_body() {
        // Output mode with a pending interaction body shows just that document
        // (the current plan) instead of the accumulated output history.
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        let mut agent = make_test_agent("run-review", AgentDisplayStatus::Waiting);
        let mut req = leviath_core::interaction::InteractionRequest::multiple_choice(
            "mc1",
            "Approve?",
            vec!["Approve".to_string()],
            "plan_approval",
        );
        req.body = Some("## Plan\n1. write the script".to_string());
        agent.pending_request = Some(req);
        dash.agents.push(agent.clone());
        dash.update_display_indices();
        assert!(dash.reviewing_body().is_some());
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
    }

    #[test]
    fn render_content_pane_inline_document_edit() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-edit", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::edit_text(
            "et1",
            "Edit",
            "plan",
            "line A\nline B",
        ));
        dash.agents.push(agent.clone());
        dash.update_display_indices();
        dash.input_mode = true;
        // The editable textarea takes over the content pane instead of output.
        assert!(dash.editing_document());
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
    }

    #[test]
    fn render_content_pane_registers_its_selection_region() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let agent = make_test_agent("run-selreg", AgentDisplayStatus::Active);
        let area = Rect::new(0, 0, 100, 20);
        terminal
            .draw(|f| dash.render_content_pane(f, area, &agent, 100))
            .unwrap();
        // Borders excluded, so mouse selection can never grab the frame or
        // the scrollbar track on the right border column.
        assert_eq!(
            dash.selection_regions,
            vec![area.inner(ratatui::layout::Margin {
                vertical: 1,
                horizontal: 1,
            })]
        );
    }

    #[test]
    fn render_content_pane_does_not_register_selection_while_editing() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-selreg-edit", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::edit_text(
            "et2", "Edit", "plan", "line A",
        ));
        dash.agents.push(agent.clone());
        dash.update_display_indices();
        dash.input_mode = true;
        assert!(dash.editing_document());
        terminal
            .draw(|f| dash.render_content_pane(f, Rect::new(0, 0, 100, 20), &agent, 100))
            .unwrap();
        // tui-textarea owns those cells; a selection there would fight the
        // editor's own cursor and highlighting.
        assert!(dash.selection_regions.is_empty());
    }

    #[test]
    fn render_content_pane_logs_mode() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Logs;
        let agent = make_test_agent("run-logs", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
    }

    #[test]
    fn render_content_pane_context_mode() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Context;
        let agent = make_test_agent("run-ctxm", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
    }

    #[test]
    fn render_content_pane_error_banner() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        let agent = make_test_agent(
            "run-err",
            AgentDisplayStatus::Error("something broke".to_string()),
        );
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
    }

    #[test]
    fn render_content_pane_error_empty_message() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        let agent = make_test_agent("run-err2", AgentDisplayStatus::Error(String::new()));
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
    }

    #[test]
    fn render_content_pane_cancelled_banner() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        let agent = make_test_agent("run-cancel", AgentDisplayStatus::Cancelled);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
    }

    #[test]
    fn render_content_pane_with_search() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        dash.search_query = "test".to_string();
        let agent = make_test_agent("run-search", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
    }

    #[test]
    fn render_content_pane_search_mode_active() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        dash.search_mode = true;
        dash.search_query = "find".to_string();
        let agent = make_test_agent("run-sm", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
    }

    #[test]
    fn render_content_pane_search_mode_active_empty_query_shows_cursor() {
        // search_mode on but no query typed yet -> the "▌" cursor indicator
        // branch (query_lc.is_empty() && self.search_mode).
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        dash.search_mode = true;
        dash.search_query = String::new();
        let agent = make_test_agent("run-sm-empty", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();

        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(content.contains('▌'));
    }

    #[test]
    fn render_content_pane_clamps_scroll_beyond_available_lines() {
        // detail_scroll set far beyond the (empty) content's max_scroll must
        // be clamped rather than underflowing/panicking.
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        dash.detail_scroll = 9999;
        let agent = make_test_agent("run-clamp", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
        assert_eq!(dash.detail_scroll, 0);
    }

    fn setup_run_state_agent_with_logs(
        run_id: &str,
        log_lines: &[&str],
        output_text: Option<&str>,
    ) -> DashboardAgent {
        let dir = runstate::run_dir(run_id);
        // Defensive cleanup: if a previous run of a test using this fixed
        // run_id panicked before reaching its own cleanup (e.g. a failed
        // assertion), stale log/output files from that run would otherwise
        // accumulate here across every subsequent `cargo test` invocation --
        // this bit us for real (a stale `logs.log` with dozens of duplicated
        // `[tool]` lines, some corrupted from concurrent-append races,
        // silently broke `render_content_pane_logs_mode_shows_tool_count_badge`'s
        // exact-count assertion on every run until this was added).
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let meta = runstate::RunMeta::new(
            run_id.to_string(),
            "agent".to_string(),
            "/p".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        runstate::create_run(&meta).unwrap();
        for line in log_lines {
            runstate::append_stage_log(run_id, 0, line);
        }
        if let Some(text) = output_text {
            runstate::append_stage_output(run_id, 0, text);
        }

        make_test_agent(run_id, AgentDisplayStatus::Active)
    }

    #[test]
    fn render_content_pane_logs_mode_shows_tool_count_badge() {
        crate::runstate::with_isolated_runs_dir(
            "render_content_pane_logs_mode_shows_tool_count_badge",
            |_d| {
                let run_id = "test-content-tool-badge";
                let agent = setup_run_state_agent_with_logs(
                    run_id,
                    &["[tool] read_file(x.rs)", "[tool] write_file(y.rs)"],
                    None,
                );

                let backend = TestBackend::new(120, 40);
                let mut terminal = Terminal::new(backend).unwrap();
                let mut dash = make_test_dashboard();
                dash.stage_content_mode = StageContentMode::Logs;
                terminal
                    .draw(|f| {
                        let area = Rect::new(0, 0, 100, 20);
                        dash.render_content_pane(f, area, &agent, 100);
                    })
                    .unwrap();

                let content: String = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                assert!(content.contains("2 tools"));

                let _ = std::fs::remove_dir_all(runstate::run_dir(run_id));
            },
        );
    }

    #[test]
    fn render_content_pane_output_mode_run_state_shows_file_path_hint() {
        crate::runstate::with_isolated_runs_dir(
            "render_content_pane_output_mode_run_state_shows_file_path_hint",
            |_d| {
                let run_id = "test-content-output-hint";
                let agent = setup_run_state_agent_with_logs(run_id, &[], Some("hello output"));

                let backend = TestBackend::new(120, 40);
                let mut terminal = Terminal::new(backend).unwrap();
                let mut dash = make_test_dashboard();
                dash.stage_content_mode = StageContentMode::Output;
                terminal
                    .draw(|f| {
                        let area = Rect::new(0, 0, 100, 20);
                        dash.render_content_pane(f, area, &agent, 100);
                    })
                    .unwrap();

                let content: String = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                assert!(content.contains("output.log"));

                let _ = std::fs::remove_dir_all(runstate::run_dir(run_id));
            },
        );
    }

    // The file-path hint's home-shortening logic is exercised directly against
    // `shorten_home_path` rather than through a full render. Reaching the raw
    // (path-outside-home) branch through the render path requires a runs dir
    // outside `$HOME`, which isn't portable: `std::env::temp_dir()` lives *under*
    // the home directory on Windows, and hard-coding `/tmp` is Unix-only.
    #[test]
    fn shorten_home_path_replaces_home_prefix_with_tilde() {
        assert_eq!(
            shorten_home_path("/home/u/.leviath/runs/x".to_string(), "/home/u"),
            "~/.leviath/runs/x"
        );
    }

    #[test]
    fn shorten_home_path_keeps_raw_path_when_outside_home() {
        // Path does not start with home -> raw branch.
        assert_eq!(
            shorten_home_path("/var/other/x".to_string(), "/home/u"),
            "/var/other/x"
        );
    }

    #[test]
    fn shorten_home_path_keeps_raw_path_when_home_empty() {
        // Empty home (dirs::home_dir() returned None) -> raw branch.
        assert_eq!(
            shorten_home_path("/anything/x".to_string(), ""),
            "/anything/x"
        );
    }

    #[test]
    fn build_output_lines_logs_mode_colors_by_line_prefix() {
        crate::runstate::with_isolated_runs_dir(
            "build_output_lines_logs_mode_colors_by_line_prefix",
            |_d| {
                let run_id = "test-content-log-prefixes";
                let agent = setup_run_state_agent_with_logs(
                    run_id,
                    &[
                        "[tool] did a thing",
                        "[error] it broke",
                        "[denied] not allowed",
                        "--- separator ---",
                        "[All stages complete]",
                        "a plain message",
                    ],
                    None,
                );

                let dash = make_test_dashboard();
                let lines = dash.build_output_lines(&agent, false, 100);
                let text: String = lines
                    .iter()
                    .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(text.contains("did a thing"));
                assert!(text.contains("it broke"));
                assert!(text.contains("not allowed"));
                assert!(text.contains("separator"));
                assert!(text.contains("All stages complete"));
                assert!(text.contains("a plain message"));

                let _ = std::fs::remove_dir_all(runstate::run_dir(run_id));
            },
        );
    }

    #[test]
    fn build_output_lines_output_mode_renders_markdown_when_non_empty() {
        crate::runstate::with_isolated_runs_dir(
            "build_output_lines_output_mode_renders_markdown_when_non_empty",
            |_d| {
                let run_id = "test-content-output-markdown";
                let agent =
                    setup_run_state_agent_with_logs(run_id, &[], Some("# Heading\n\nbody text"));

                let dash = make_test_dashboard();
                let lines = dash.build_output_lines(&agent, true, 100);
                assert!(!lines.is_empty());
                let text: String = lines
                    .iter()
                    .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(text.contains("Heading"));
                assert!(text.contains("body text"));

                let _ = std::fs::remove_dir_all(runstate::run_dir(run_id));
            },
        );
    }

    #[test]
    fn build_context_lines_with_snapshot() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-bcl", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(make_context_snapshot(4000, 8000));
        let lines = dash.build_context_lines(&agent, 80);
        assert!(!lines.is_empty());
    }

    #[test]
    fn build_context_lines_without_snapshot() {
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-bcl2", AgentDisplayStatus::Active);
        let lines = dash.build_context_lines(&agent, 80);
        assert!(!lines.is_empty()); // should show "no context snapshot" message
    }

    #[test]
    fn build_context_lines_with_graph_info() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-graph", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(make_context_snapshot(4000, 8000));
        let mut edges = std::collections::HashMap::new();
        edges.insert(
            "main".to_string(),
            vec![crate::commands::dashboard::graph::GraphEdge {
                target: "implement".to_string(),
                hint: Some("after plan".to_string()),
                condition: "always".to_string(),
                transform: "replace".to_string(),
            }],
        );
        agent.graph_info = Some(crate::commands::dashboard::graph::GraphTransitionInfo {
            edges,
            entry_stage: "main".to_string(),
            stage_names: vec!["main".to_string(), "implement".to_string()],
        });
        agent.stages = vec![crate::runstate::StageRecord {
            name: "main".to_string(),
            index: 0,
            status: crate::runstate::StageRunStatus::Active,
            prompt_tokens: 100,
            completion_tokens: 50,
            cached_tokens: 0,
            started_at: Some(chrono::Utc::now().timestamp() - 30),
            ended_at: None,
        }];
        let lines = dash.build_context_lines(&agent, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("Stage:"));
    }

    #[test]
    fn build_context_lines_graph_info_falls_back_to_stage_names_when_no_stage_record() {
        // agent.stages doesn't have an entry for the selected index, so
        // sel_name must fall back to graph.stage_names.get(selected_stage).
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-graph-fallback", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(make_context_snapshot(4000, 8000));
        let edges = std::collections::HashMap::new();
        agent.graph_info = Some(crate::commands::dashboard::graph::GraphTransitionInfo {
            edges,
            entry_stage: "main".to_string(),
            stage_names: vec!["main".to_string(), "implement".to_string()],
        });
        agent.stages = vec![]; // no stage records at all -> .get(0) is None

        let lines = dash.build_context_lines(&agent, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("Stage: main"));
    }

    #[test]
    fn build_context_lines_graph_info_shows_visited_count() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-graph-visited", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(make_context_snapshot(4000, 8000));
        agent.graph_info = Some(crate::commands::dashboard::graph::GraphTransitionInfo {
            edges: std::collections::HashMap::new(),
            entry_stage: "main".to_string(),
            stage_names: vec!["main".to_string()],
        });
        // Two records named "main" -> visited count 2, exercising the plural "s".
        let rec = crate::runstate::StageRecord {
            name: "main".to_string(),
            index: 0,
            status: crate::runstate::StageRunStatus::Active,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            started_at: Some(chrono::Utc::now().timestamp() - 30),
            ended_at: None,
        };
        agent.stages = vec![rec.clone(), rec];

        let lines = dash.build_context_lines(&agent, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("Visited 2 times"));
    }

    #[test]
    fn build_context_lines_graph_info_edge_with_non_always_condition() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-graph-cond", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(make_context_snapshot(4000, 8000));
        let mut edges = std::collections::HashMap::new();
        edges.insert(
            "main".to_string(),
            vec![crate::commands::dashboard::graph::GraphEdge {
                target: "error_recovery".to_string(),
                hint: None,
                condition: "error".to_string(),
                transform: "direct".to_string(),
            }],
        );
        agent.graph_info = Some(crate::commands::dashboard::graph::GraphTransitionInfo {
            edges,
            entry_stage: "main".to_string(),
            stage_names: vec!["main".to_string(), "error_recovery".to_string()],
        });
        agent.stages = vec![crate::runstate::StageRecord {
            name: "main".to_string(),
            index: 0,
            status: crate::runstate::StageRunStatus::Active,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            started_at: Some(chrono::Utc::now().timestamp() - 30),
            ended_at: None,
        }];

        let lines = dash.build_context_lines(&agent, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("[error]"));
    }

    #[test]
    fn build_output_lines_non_run_state() {
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-nrs", AgentDisplayStatus::Active);
        // is_run_state is false, so content will be empty
        let lines = dash.build_output_lines(&agent, true, 80);
        assert!(lines.is_empty());
    }

    #[test]
    fn build_context_lines_with_entries() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-entries", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 4000,
            max_tokens: 8000,
            regions: vec![runstate::RegionSnapshot {
                name: "system".to_string(),
                kind: "pinned".to_string(),
                current_tokens: 2000,
                max_tokens: 4000,
                entries: vec![runstate::RegionEntrySnapshot {
                    content: "Hello world".to_string(),
                    tokens: 5,
                    kind: Default::default(),
                    metadata: None,
                    key: None,
                    taint: Default::default(),
                }],
            }],
        });
        let lines = dash.build_context_lines(&agent, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("entry 1"));
    }

    #[test]
    fn build_context_lines_old_run_without_entries() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-old", AgentDisplayStatus::Active);
        // Has tokens but no entries = old run
        agent.context_snapshot = Some(runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 4000,
            max_tokens: 8000,
            regions: vec![runstate::RegionSnapshot {
                name: "system".to_string(),
                kind: "pinned".to_string(),
                current_tokens: 2000,
                max_tokens: 4000,
                entries: vec![],
            }],
        });
        let lines = dash.build_context_lines(&agent, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("predates"));
    }

    // ─── build_context_lines: incoming edges in graph ─────────────────────

    #[test]
    fn build_context_lines_with_graph_info_and_incoming_edges() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-incoming", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(make_context_snapshot(4000, 8000));
        let mut edges = std::collections::HashMap::new();
        // "plan" has edge to "implement"
        edges.insert(
            "plan".to_string(),
            vec![crate::commands::dashboard::graph::GraphEdge {
                target: "implement".to_string(),
                hint: None,
                condition: "always".to_string(),
                transform: "replace".to_string(),
            }],
        );
        // "implement" is selected stage — it has an incoming edge from "plan"
        agent.graph_info = Some(crate::commands::dashboard::graph::GraphTransitionInfo {
            edges,
            entry_stage: "plan".to_string(),
            stage_names: vec!["plan".to_string(), "implement".to_string()],
        });
        agent.stages = vec![crate::runstate::StageRecord {
            name: "implement".to_string(),
            index: 1,
            status: crate::runstate::StageRunStatus::Active,
            prompt_tokens: 100,
            completion_tokens: 50,
            cached_tokens: 0,
            started_at: Some(chrono::Utc::now().timestamp() - 30),
            ended_at: None,
        }];
        // selected_stage = 0, so we look up index 0 in stages which is "implement"
        let lines = dash.build_context_lines(&agent, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        // Should show stage info
        assert!(!lines.is_empty());
        // The incoming edge from "plan" should be listed
        assert!(text.contains("← plan"));
    }

    #[test]
    fn build_context_lines_with_terminal_stage_no_edges() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-terminal", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(make_context_snapshot(4000, 8000));
        let mut edges = std::collections::HashMap::new();
        // "implement" has no outgoing edges (terminal)
        edges.insert("plan".to_string(), vec![]);
        agent.graph_info = Some(crate::commands::dashboard::graph::GraphTransitionInfo {
            edges,
            entry_stage: "plan".to_string(),
            stage_names: vec!["plan".to_string()],
        });
        agent.stages = vec![crate::runstate::StageRecord {
            name: "plan".to_string(),
            index: 0,
            status: crate::runstate::StageRunStatus::Complete,
            prompt_tokens: 100,
            completion_tokens: 50,
            cached_tokens: 0,
            started_at: Some(chrono::Utc::now().timestamp() - 60),
            ended_at: Some(chrono::Utc::now().timestamp() - 10),
        }];
        let lines = dash.build_context_lines(&agent, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("terminal"));
    }

    #[test]
    fn build_context_lines_with_no_edges_for_stage() {
        // Stage has no entry in edges map at all → shows "linear — no graph edges"
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-noedge", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(make_context_snapshot(4000, 8000));
        let edges = std::collections::HashMap::new(); // empty
        agent.graph_info = Some(crate::commands::dashboard::graph::GraphTransitionInfo {
            edges,
            entry_stage: "main".to_string(),
            stage_names: vec!["main".to_string()],
        });
        agent.stages = vec![crate::runstate::StageRecord {
            name: "main".to_string(),
            index: 0,
            status: crate::runstate::StageRunStatus::Active,
            prompt_tokens: 100,
            completion_tokens: 50,
            cached_tokens: 0,
            started_at: Some(chrono::Utc::now().timestamp() - 30),
            ended_at: None,
        }];
        let lines = dash.build_context_lines(&agent, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(text.contains("linear"));
    }

    // ─── build_context_lines: multiple region kinds ───────────────────────

    #[test]
    fn build_context_lines_all_region_kinds() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-kinds", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 6000,
            max_tokens: 8000,
            regions: vec![
                runstate::RegionSnapshot {
                    name: "system".to_string(),
                    kind: "pinned".to_string(),
                    current_tokens: 1000,
                    max_tokens: 2000,
                    entries: vec![],
                },
                runstate::RegionSnapshot {
                    name: "conv".to_string(),
                    kind: "sliding".to_string(),
                    current_tokens: 1000,
                    max_tokens: 2000,
                    entries: vec![],
                },
                runstate::RegionSnapshot {
                    name: "hist".to_string(),
                    kind: "history".to_string(),
                    current_tokens: 1000,
                    max_tokens: 2000,
                    entries: vec![],
                },
                runstate::RegionSnapshot {
                    name: "temp".to_string(),
                    kind: "temporary".to_string(),
                    current_tokens: 1000,
                    max_tokens: 2000,
                    entries: vec![],
                },
                runstate::RegionSnapshot {
                    name: "cls".to_string(),
                    kind: "clearable".to_string(),
                    current_tokens: 1000,
                    max_tokens: 2000,
                    entries: vec![],
                },
                runstate::RegionSnapshot {
                    name: "other".to_string(),
                    kind: "unknown_kind".to_string(),
                    current_tokens: 1000,
                    max_tokens: 2000,
                    entries: vec![],
                },
            ],
        });
        let lines = dash.build_context_lines(&agent, 80);
        assert!(!lines.is_empty());
    }

    // ─── build_context_lines: 90%+ bar color (error red) ─────────────────

    #[test]
    fn build_context_lines_high_usage_region() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-hiuse", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 7500,
            max_tokens: 8000,
            regions: vec![runstate::RegionSnapshot {
                name: "system".to_string(),
                kind: "pinned".to_string(),
                current_tokens: 7500,
                max_tokens: 8000,
                entries: vec![],
            }],
        });
        let lines = dash.build_context_lines(&agent, 80);
        assert!(!lines.is_empty());
    }

    // ─── build_context_lines: 70-90% bar (warning yellow) ────────────────

    #[test]
    fn build_context_lines_medium_usage_region() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-meduse", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 5800,
            max_tokens: 8000,
            regions: vec![runstate::RegionSnapshot {
                name: "system".to_string(),
                kind: "compacting".to_string(),
                current_tokens: 5800,
                max_tokens: 8000,
                entries: vec![],
            }],
        });
        let lines = dash.build_context_lines(&agent, 80);
        assert!(!lines.is_empty());
    }

    // ─── build_context_lines: zero usage region ───────────────────────────

    #[test]
    fn build_context_lines_zero_usage_region() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-zerouse", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 0,
            max_tokens: 8000,
            regions: vec![runstate::RegionSnapshot {
                name: "empty".to_string(),
                kind: "sliding".to_string(),
                current_tokens: 0,
                max_tokens: 8000,
                entries: vec![],
            }],
        });
        let lines = dash.build_context_lines(&agent, 80);
        assert!(!lines.is_empty());
    }

    // ─── render_content_pane: with active search producing matches ─────────

    #[test]
    fn render_content_pane_search_with_matches() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        // We need actual content lines to match against — build_output_lines
        // will return empty for non-run-state, so use context mode with a snapshot
        // that has entries containing "hello"
        dash.stage_content_mode = StageContentMode::Context;
        dash.search_query = "token".to_string();
        let mut agent = make_test_agent("run-sm2", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 4000,
            max_tokens: 8000,
            regions: vec![runstate::RegionSnapshot {
                name: "system".to_string(),
                kind: "pinned".to_string(),
                current_tokens: 2000,
                max_tokens: 4000,
                entries: vec![runstate::RegionEntrySnapshot {
                    content: "hello token world".to_string(),
                    tokens: 5,
                    kind: Default::default(),
                    metadata: None,
                    key: None,
                    taint: Default::default(),
                }],
            }],
        });
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
    }

    // ─── render_content_pane: scroll at bottom (detail_scroll = 0) ────────

    #[test]
    fn render_content_pane_scrollbar_visible_when_overflow() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Context;
        dash.detail_scroll = 5;
        let mut agent = make_test_agent("run-scroll", AgentDisplayStatus::Active);
        // Make snapshot with many entries to exceed screen height
        let entries: Vec<runstate::RegionEntrySnapshot> = (0..50)
            .map(|i| runstate::RegionEntrySnapshot {
                content: format!("content line {}", i),
                tokens: 10,
                kind: Default::default(),
                metadata: None,
                key: None,
                taint: Default::default(),
            })
            .collect();
        agent.context_snapshot = Some(runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 4000,
            max_tokens: 8000,
            regions: vec![runstate::RegionSnapshot {
                name: "big".to_string(),
                kind: "sliding".to_string(),
                current_tokens: 4000,
                max_tokens: 8000,
                entries,
            }],
        });
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
    }

    // ─── build_output_lines: logs mode with prefixed lines ────────────────

    #[test]
    fn build_output_lines_logs_mode_non_run_state() {
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-logs-nrs", AgentDisplayStatus::Active);
        // non-run-state, is_output = false
        let lines = dash.build_output_lines(&agent, false, 80);
        // Should be empty because no disk content
        assert!(lines.is_empty());
    }

    // ─── render_content_pane: stage_name from stages list ─────────────────

    #[test]
    fn render_content_pane_with_stage_name_in_title() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Output;
        let mut agent = make_test_agent("run-sn", AgentDisplayStatus::Active);
        agent.stages = vec![crate::runstate::StageRecord {
            name: "analyze".to_string(),
            index: 0,
            status: crate::runstate::StageRunStatus::Active,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            started_at: None,
            ended_at: None,
        }];
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 10);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
    }

    // ─── render_content_pane: Logs mode with tool count (run_state) ──────

    #[test]
    fn render_content_pane_logs_mode_run_state() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Logs;
        // is_run_state = true means it tries to read disk, but dir won't exist
        // so it returns empty content gracefully
        let agent = make_test_agent("run-logs-rs", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
    }

    // ─── render_context_bar: bar color variants ───────────────────────────

    #[test]
    fn render_context_bar_regions_string_with_many_region_types() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-regions", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 4000,
            max_tokens: 8000,
            regions: vec![
                runstate::RegionSnapshot {
                    name: "s1".to_string(),
                    kind: "pinned".to_string(),
                    current_tokens: 500,
                    max_tokens: 1000,
                    entries: vec![],
                },
                runstate::RegionSnapshot {
                    name: "s2".to_string(),
                    kind: "sliding".to_string(),
                    current_tokens: 500,
                    max_tokens: 1000,
                    entries: vec![],
                },
                runstate::RegionSnapshot {
                    name: "s3".to_string(),
                    kind: "compacting".to_string(),
                    current_tokens: 500,
                    max_tokens: 1000,
                    entries: vec![],
                },
                runstate::RegionSnapshot {
                    name: "s4".to_string(),
                    kind: "history".to_string(),
                    current_tokens: 500,
                    max_tokens: 1000,
                    entries: vec![],
                },
                runstate::RegionSnapshot {
                    name: "s5".to_string(),
                    kind: "other".to_string(),
                    current_tokens: 500,
                    max_tokens: 1000,
                    entries: vec![],
                },
                runstate::RegionSnapshot {
                    name: "s6".to_string(),
                    kind: "more".to_string(),
                    current_tokens: 500,
                    max_tokens: 1000,
                    entries: vec![],
                },
                runstate::RegionSnapshot {
                    name: "s7".to_string(),
                    kind: "extra".to_string(),
                    current_tokens: 500,
                    max_tokens: 1000,
                    entries: vec![],
                },
            ],
        });
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 5);
                dash.render_context_bar(f, area, &agent);
            })
            .unwrap();
    }

    // ─── render_context_bar: run_state agent uses stage context ──────────

    #[test]
    fn render_context_bar_run_state_uses_stage_context() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-rs-ctx", AgentDisplayStatus::Active);
        // is_run_state = true, context_snapshot as fallback
        agent.context_snapshot = Some(make_context_snapshot(3000, 8000));
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 5);
                dash.render_context_bar(f, area, &agent);
            })
            .unwrap();
    }

    /// A one-point context history for the browsing render tests.
    fn one_point_history(
        context: runstate::ContextSnapshot,
    ) -> Vec<leviath_core::run_archive::RunPoint> {
        vec![leviath_core::run_archive::RunPoint {
            meta: leviath_core::run_meta::RunMeta::new(
                "r".to_string(),
                "a".to_string(),
                "/p".to_string(),
                "t".to_string(),
                None,
                "/w".to_string(),
                1,
            ),
            context,
            at: 1,
        }]
    }

    #[test]
    fn render_context_bar_shows_history_position_when_browsing() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let agent = make_test_agent("run-hist-bar", AgentDisplayStatus::Active);
        dash.context_history = one_point_history(make_context_snapshot(1000, 8000));
        dash.context_history_idx = Some(0);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 80, 5);
                dash.render_context_bar(f, area, &agent);
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            text.contains("ctx 1/1"),
            "history position in title: {text}"
        );
    }

    #[test]
    fn build_context_lines_uses_browsed_history_point() {
        let mut dash = make_test_dashboard();
        // No live snapshot on the agent → the browsed point is the only source.
        let agent = make_test_agent("run-hist-lines-xyzzy", AgentDisplayStatus::Active);
        dash.context_history = one_point_history(runstate::ContextSnapshot {
            stage_name: "browsed-stage".to_string(),
            total_tokens: 42,
            max_tokens: 100,
            regions: vec![runstate::RegionSnapshot {
                name: "hist-region".to_string(),
                kind: "pinned".to_string(),
                current_tokens: 42,
                max_tokens: 100,
                entries: vec![],
            }],
        });
        dash.context_history_idx = Some(0);
        let lines = dash.build_context_lines(&agent, 80);
        let text: String = lines.iter().map(|l| format!("{l:?}")).collect();
        assert!(text.contains("hist-region"), "browsed region rendered");
    }

    // ─── render_content_pane: Context mode with is_run_state (disk fallback)

    #[test]
    fn render_content_pane_context_mode_run_state() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Context;
        let agent = make_test_agent("run-ctx-rs", AgentDisplayStatus::Active);
        // No context_snapshot so it shows "no context snapshot available"
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
    }

    // ─── render_content_pane: file path hint for context mode ─────────────

    #[test]
    fn render_content_pane_file_path_hint_context_mode() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Context;
        let agent = make_test_agent("run-ctx-fph", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
    }

    // ─── render_content_pane: file path hint for Logs mode ───────────────

    #[test]
    fn render_content_pane_file_path_hint_logs_mode() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Logs;
        let agent = make_test_agent("run-logs-fph", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
    }

    // ─── render_content_pane: search with no matches shows 0 matches ──────

    #[test]
    fn render_content_pane_search_no_matches() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        dash.stage_content_mode = StageContentMode::Context;
        dash.search_query = "xyznotfound".to_string();
        let mut agent = make_test_agent("run-nm", AgentDisplayStatus::Active);
        agent.context_snapshot = Some(make_context_snapshot(4000, 8000));
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 100, 20);
                dash.render_content_pane(f, area, &agent, 100);
            })
            .unwrap();
    }
}
