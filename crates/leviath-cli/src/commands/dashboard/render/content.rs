//! Content pane rendering: output, logs, context view, search highlighting.

use ratatui::layout::{Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use ratatui::Frame;

use crate::commands::dashboard::helpers::format_tokens;
use crate::commands::dashboard::state::Dashboard;
use crate::commands::dashboard::theme::*;
use crate::commands::dashboard::types::*;
use crate::runstate;

impl Dashboard {
    pub(in crate::commands::dashboard) fn render_context_bar(
        &self,
        frame: &mut Frame,
        ctx_area: Rect,
        agent: &DashboardAgent,
    ) {
        // Use per-stage context if available, else fall back to global snapshot
        let snap_opt = if agent.is_run_state {
            runstate::read_stage_context(&agent.id, self.selected_stage)
                .or_else(|| agent.context_snapshot.clone())
        } else {
            agent.context_snapshot.clone()
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
                        .title(Span::styled(" ctx ", Style::default().fg(C_DIM)))
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
                        .title(Span::styled(" ctx ", Style::default().fg(C_DIM)))
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
        let tool_count = if self.stage_content_mode == StageContentMode::Logs && agent.is_run_state
        {
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
        let file_path_hint = if agent.is_run_state {
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
            let shortened = if !home.is_empty() && raw.starts_with(&home) {
                format!("~{}", &raw[home.len()..])
            } else {
                raw
            };
            format!(" {} ", shortened)
        } else {
            String::new()
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
        let snap_opt = if agent.is_run_state {
            runstate::read_stage_context(&agent.id, self.selected_stage)
                .or_else(|| agent.context_snapshot.clone())
        } else {
            agent.context_snapshot.clone()
        };
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

    fn build_output_lines(
        &self,
        agent: &DashboardAgent,
        is_output: bool,
        render_width: u16,
    ) -> Vec<Line<'static>> {
        let content = if agent.is_run_state {
            if is_output {
                runstate::tail_stage_output(&agent.id, self.selected_stage, 131_072)
            } else {
                runstate::tail_stage_log(&agent.id, self.selected_stage, 131_072)
            }
        } else {
            String::new()
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
