//! Stage tab bar and graph view rendering.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Tabs};
use ratatui::Frame;

use crate::commands::dashboard::graph::{GraphEdge, GraphTransitionInfo};
use crate::commands::dashboard::helpers::truncate;
use crate::commands::dashboard::state::Dashboard;
use crate::commands::dashboard::theme::*;
use crate::commands::dashboard::types::*;
use crate::runstate::StageRunStatus;

impl Dashboard {
    pub(in crate::commands::dashboard) fn render_stage_tabs(
        &self,
        frame: &mut Frame,
        tabs_area: Rect,
        agent: &DashboardAgent,
    ) {
        if let Some(ref graph) = agent.graph_info {
            // ── Graph view: render stages as boxes with arrows ──────────────
            self.draw_graph_view(frame, tabs_area, agent, graph);
        } else {
            // ── Linear tabs view ───────────────────────────────────────────
            self.draw_linear_tabs(frame, tabs_area, agent);
        }
    }

    fn draw_linear_tabs(&self, frame: &mut Frame, tabs_area: Rect, agent: &DashboardAgent) {
        // Build tab titles with status glyphs
        let tab_titles: Vec<Line> = if agent.stages.is_empty() {
            // Fallback: synthesize stage names from RunMeta info
            (0..agent.num_stages.max(1))
                .map(|i| {
                    let glyph = if i < agent.stage_index {
                        Span::styled(
                            format!("{} ", GLYPH_COMPLETE),
                            Style::default().fg(C_SUCCESS),
                        )
                    } else if i == agent.stage_index {
                        match &agent.status {
                            AgentDisplayStatus::Active => Span::styled(
                                format!("{} ", SPINNER[(self.tick_count as usize) % SPINNER.len()]),
                                Style::default().fg(C_ACTIVE),
                            ),
                            AgentDisplayStatus::Waiting => Span::styled(
                                format!("{} ", GLYPH_WAITING),
                                Style::default().fg(C_WARN),
                            ),
                            AgentDisplayStatus::Error(_) => Span::styled(
                                format!("{} ", GLYPH_ERROR),
                                Style::default().fg(C_ERROR),
                            ),
                            _ => Span::styled(
                                format!("{} ", GLYPH_COMPLETE),
                                Style::default().fg(C_SUCCESS),
                            ),
                        }
                    } else {
                        Span::styled(format!("{} ", GLYPH_PENDING), Style::default().fg(C_DIM))
                    };
                    let stage_label = if i == agent.stage_index {
                        truncate(&agent.stage, 12)
                    } else {
                        format!("stage {}", i + 1)
                    };
                    let label_span = if i == agent.stage_index {
                        Span::styled(
                            stage_label,
                            Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                        )
                    } else {
                        Span::styled(stage_label, Style::default().fg(C_MUTED))
                    };
                    // Live stage marker
                    let live_marker = if i == agent.stage_index
                        && !matches!(
                            agent.status,
                            AgentDisplayStatus::Complete
                                | AgentDisplayStatus::CompleteInteractive
                                | AgentDisplayStatus::Cancelled
                                | AgentDisplayStatus::Error(_)
                        ) {
                        Span::styled("*", Style::default().fg(C_WARN))
                    } else {
                        Span::raw("")
                    };
                    Line::from(vec![glyph, label_span, live_marker])
                })
                .collect()
        } else {
            agent
                .stages
                .iter()
                .enumerate()
                .map(|(i, s)| self.build_stage_tab_title(i, s, agent))
                .collect()
        };

        let tabs_count = tab_titles.len().max(1);
        let selected_tab = self.selected_stage.min(tabs_count - 1);

        let tab_nav = if tabs_count > 1 {
            format!(" ←/→ to switch  stage {}/{}", selected_tab + 1, tabs_count)
        } else {
            " stage 1/1".to_string()
        };

        let tabs_widget = Tabs::new(tab_titles)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(C_BORDER_FOCUS))
                    .title(Span::styled(
                        format!(" Stages{}", tab_nav),
                        Style::default().fg(C_DIM),
                    )),
            )
            .select(selected_tab)
            .highlight_style(
                Style::default()
                    .fg(C_ACCENT)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            )
            .divider(Span::styled(" │ ", Style::default().fg(C_DIM)));

        frame.render_widget(tabs_widget, tabs_area);
    }

    fn build_stage_tab_title(
        &self,
        i: usize,
        s: &crate::runstate::StageRecord,
        agent: &DashboardAgent,
    ) -> Line<'static> {
        use crate::commands::dashboard::helpers::{elapsed_str, elapsed_str_until};

        // Compute stage duration string
        let dur_str = match (s.started_at, s.ended_at) {
            (Some(start), Some(end)) => {
                let secs = (end - start).max(0) as u64;
                if secs < 60 {
                    format!(" {}s", secs)
                } else {
                    format!(" {}m{}s", secs / 60, secs % 60)
                }
            }
            (Some(start), None) if s.status == StageRunStatus::Active => {
                let effective_start = start + agent.waiting_secs as i64;
                let dur = if let Some(until) = agent.active_until {
                    elapsed_str_until(effective_start, until)
                } else {
                    elapsed_str(effective_start)
                };
                format!(" {}", dur)
            }
            _ => String::new(),
        };

        let (glyph, glyph_style) = match &s.status {
            StageRunStatus::Pending => (GLYPH_PENDING, Style::default().fg(C_DIM)),
            StageRunStatus::Active => {
                let run_done = matches!(
                    agent.status,
                    AgentDisplayStatus::Complete
                        | AgentDisplayStatus::CompleteInteractive
                        | AgentDisplayStatus::Cancelled
                        | AgentDisplayStatus::Error(_)
                );
                if run_done {
                    (GLYPH_COMPLETE, Style::default().fg(C_SUCCESS))
                } else {
                    let spin = SPINNER[(self.tick_count as usize) % SPINNER.len()];
                    return Line::from(vec![
                        Span::styled(format!("{} ", spin), Style::default().fg(C_ACTIVE)),
                        Span::styled(
                            truncate(&s.name, 10),
                            Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled("*", Style::default().fg(C_WARN)),
                        Span::styled(dur_str, Style::default().fg(C_DIM)),
                    ]);
                }
            }
            StageRunStatus::WaitingInput => (GLYPH_WAITING, Style::default().fg(C_WARN)),
            StageRunStatus::Complete => (GLYPH_COMPLETE, Style::default().fg(C_SUCCESS)),
            StageRunStatus::Error => (GLYPH_ERROR, Style::default().fg(C_ERROR)),
        };
        let is_live = i == agent.stage_index
            && !matches!(
                agent.status,
                AgentDisplayStatus::Complete
                    | AgentDisplayStatus::CompleteInteractive
                    | AgentDisplayStatus::Cancelled
                    | AgentDisplayStatus::Error(_)
            );
        let label_style = if is_live {
            Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(C_MUTED)
        };
        Line::from(vec![
            Span::styled(format!("{} ", glyph), glyph_style),
            Span::styled(truncate(&s.name, 10), label_style),
            Span::styled(dur_str, Style::default().fg(C_DIM)),
        ])
    }

    /// Render the graph view of stages in the tabs area.
    fn draw_graph_view(
        &self,
        frame: &mut Frame,
        area: Rect,
        agent: &DashboardAgent,
        graph: &GraphTransitionInfo,
    ) {
        // Determine visit counts and stage statuses from stage records
        let mut visit_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut stage_statuses: std::collections::HashMap<String, &StageRunStatus> =
            std::collections::HashMap::new();
        for s in &agent.stages {
            *visit_counts.entry(s.name.clone()).or_default() += 1;
            stage_statuses.insert(s.name.clone(), &s.status);
        }

        let current_stage = &agent.stage;
        let run_done = matches!(
            agent.status,
            AgentDisplayStatus::Complete
                | AgentDisplayStatus::CompleteInteractive
                | AgentDisplayStatus::Cancelled
                | AgentDisplayStatus::Error(_)
        );

        // Determine reachable stages from current position
        let reachable = {
            let mut set = std::collections::HashSet::new();
            let mut queue = vec![current_stage.as_str()];
            while let Some(name) = queue.pop() {
                if !set.insert(name.to_string()) {
                    continue;
                }
                if let Some(edges) = graph.edges.get(name) {
                    for edge in edges {
                        if !set.contains(&edge.target) {
                            queue.push(
                                graph
                                    .stage_names
                                    .iter()
                                    .find(|s| **s == edge.target)
                                    .map(|s| s.as_str())
                                    .unwrap_or(""),
                            );
                        }
                    }
                }
            }
            set
        };

        // Determine which stages to show
        let visible_stages: Vec<&String> = graph
            .stage_names
            .iter()
            .filter(|name| {
                visit_counts.contains_key(name.as_str())
                    || reachable.contains(name.as_str())
                    || **name == graph.entry_stage
            })
            .collect();

        if visible_stages.is_empty() {
            frame.render_widget(
                Paragraph::new(" No stages yet.")
                    .style(Style::default().fg(C_DIM))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(C_BORDER)),
                    ),
                area,
            );
            return;
        }

        // Compute node widths
        let node_widths: Vec<usize> = visible_stages
            .iter()
            .map(|name| {
                let vc = visit_counts.get(name.as_str()).copied().unwrap_or(0);
                let count_suffix = if vc > 1 {
                    format!(" x{}", vc)
                } else {
                    String::new()
                };
                name.len() + count_suffix.len() + 4
            })
            .collect();

        let arrow_w = 3usize;
        let inner_w = area.width.saturating_sub(2) as usize;

        let mut line1_spans: Vec<Span> = Vec::new();
        let mut line2_spans: Vec<Span> = Vec::new();
        let mut line3_spans: Vec<Span> = Vec::new();
        let mut line4_spans: Vec<Span> = Vec::new();
        let mut total_w = 0usize;

        for (i, name) in visible_stages.iter().enumerate() {
            let vc = visit_counts.get(name.as_str()).copied().unwrap_or(0);
            let count_suffix = if vc > 1 {
                format!(" x{}", vc)
            } else {
                String::new()
            };

            let nw = node_widths[i];
            let label_w = nw - 4;

            let is_current = *name == current_stage && !run_done;
            let node_color = if is_current {
                C_ACCENT
            } else if let Some(status) = stage_statuses.get(name.as_str()) {
                match status {
                    StageRunStatus::Complete => C_SUCCESS,
                    StageRunStatus::Error => C_ERROR,
                    StageRunStatus::Active if run_done => C_SUCCESS,
                    StageRunStatus::Active => C_ACCENT,
                    StageRunStatus::WaitingInput => C_WARN,
                    StageRunStatus::Pending => C_DIM,
                }
            } else if !reachable.contains(name.as_str()) {
                C_DIM
            } else {
                C_MUTED
            };

            let border_mod = if is_current {
                Modifier::BOLD
            } else {
                Modifier::empty()
            };
            let border_style = Style::default().fg(node_color).add_modifier(border_mod);

            let top = format!("┌{}┐", "─".repeat(nw - 2));
            line1_spans.push(Span::styled(top, border_style));

            let label_style = if is_current {
                Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(node_color)
            };
            line2_spans.push(Span::styled("│".to_string(), border_style));
            line2_spans.push(Span::styled(
                format!(
                    " {:<width$}",
                    format!("{}{}", name, count_suffix),
                    width = label_w
                ),
                label_style,
            ));
            line2_spans.push(Span::styled("│".to_string(), border_style));

            let bottom = format!("└{}┘", "─".repeat(nw - 2));
            line3_spans.push(Span::styled(bottom, border_style));

            let glyph = if is_current {
                let spin = SPINNER[(self.tick_count as usize) % SPINNER.len()];
                Span::styled(
                    format!("{:^width$}", spin, width = nw),
                    Style::default().fg(C_ACCENT),
                )
            } else if vc > 0 {
                let g = if stage_statuses
                    .get(name.as_str())
                    .is_some_and(|s| matches!(s, StageRunStatus::Error))
                {
                    GLYPH_ERROR
                } else {
                    GLYPH_COMPLETE
                };
                Span::styled(
                    format!("{:^width$}", g, width = nw),
                    Style::default().fg(node_color),
                )
            } else {
                Span::styled(
                    format!("{:^width$}", GLYPH_PENDING, width = nw),
                    Style::default().fg(C_DIM),
                )
            };
            line4_spans.push(glyph);

            total_w += nw;

            if i < visible_stages.len() - 1 {
                let next_name = visible_stages[i + 1];
                let has_edge = graph
                    .edges
                    .get(name.as_str())
                    .is_some_and(|edges| edges.iter().any(|e| e.target == **next_name));
                let has_reverse = graph
                    .edges
                    .get(next_name.as_str())
                    .is_some_and(|edges| edges.iter().any(|e| e.target == **name));

                let arrow = if has_edge && has_reverse {
                    "←→"
                } else if has_edge {
                    "──→"
                } else if has_reverse {
                    "←──"
                } else {
                    "   "
                };
                let arrow_color = if has_edge || has_reverse {
                    C_MUTED
                } else {
                    C_DIM
                };
                line1_spans.push(Span::styled(
                    " ".repeat(arrow_w),
                    Style::default().fg(C_DIM),
                ));
                line2_spans.push(Span::styled(
                    arrow.to_string(),
                    Style::default().fg(arrow_color),
                ));
                line3_spans.push(Span::styled(
                    " ".repeat(arrow_w),
                    Style::default().fg(C_DIM),
                ));
                line4_spans.push(Span::styled(
                    " ".repeat(arrow_w),
                    Style::default().fg(C_DIM),
                ));
                total_w += arrow_w;
            }

            if total_w > inner_w {
                break;
            }
        }

        // Selected stage info line
        let selected_name = visible_stages
            .get(self.selected_stage)
            .map(|s| s.as_str())
            .unwrap_or(current_stage);
        let selected_edges = graph.edges.get(selected_name);
        let edge_summary = Self::build_edge_summary(selected_edges);

        let nav_hint = format!(
            " ←/→ select  stage {}/{}{}",
            self.selected_stage + 1,
            visible_stages.len(),
            edge_summary,
        );

        let graph_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(C_BORDER_FOCUS))
            .title(Span::styled(" Stage Graph ", Style::default().fg(C_ACCENT)));

        let lines = vec![
            Line::from(line1_spans),
            Line::from(line2_spans),
            Line::from(line3_spans),
            Line::from(line4_spans),
            Line::from(Span::styled(nav_hint, Style::default().fg(C_DIM))),
        ];
        let widget = Paragraph::new(lines).block(graph_block);
        frame.render_widget(widget, area);
    }

    fn build_edge_summary(selected_edges: Option<&Vec<GraphEdge>>) -> String {
        if let Some(edges) = selected_edges {
            let parts: Vec<String> = edges
                .iter()
                .filter(|e| e.condition != "error")
                .map(|e| {
                    let hint_part = e
                        .hint
                        .as_deref()
                        .map(|h| format!("({})", truncate(h, 20)))
                        .unwrap_or_default();
                    format!("→{}{}", e.target, hint_part)
                })
                .collect();
            if parts.is_empty() {
                " (terminal)".to_string()
            } else {
                format!("  {}", parts.join("  "))
            }
        } else {
            String::new()
        }
    }
}
