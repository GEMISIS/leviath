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

        // Only the tab matching the agent's current stage index may show live
        // (spinner / ticking-duration) treatment. A stage record can be left
        // stuck at `Active` (e.g. a prior stage whose completion was never
        // recorded) even though the run has moved on — render those as
        // Complete instead of animating a spinner on a stage that isn't
        // actually running.
        let is_current_tab = i == agent.stage_index;

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
            (Some(start), None) if s.status == StageRunStatus::Active && is_current_tab => {
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
                if run_done || !is_current_tab {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runstate::{StageRecord, StageRunStatus};
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    use std::collections::HashMap;
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
            model: None,
            parent_id: None,
            depth: 0,
            started_at: chrono::Utc::now().timestamp() - 60,
            active_until: None,
            waiting_secs: 0,
            graph_info: None,
            accepts_messages: true,
        }
    }

    fn make_stage_record(name: &str, status: StageRunStatus) -> StageRecord {
        StageRecord {
            name: name.to_string(),
            index: 0,
            status: status.clone(),
            prompt_tokens: 100,
            completion_tokens: 50,
            cached_tokens: 0,
            started_at: Some(chrono::Utc::now().timestamp() - 30),
            ended_at: if status == StageRunStatus::Complete {
                Some(chrono::Utc::now().timestamp())
            } else {
                None
            },
        }
    }

    #[test]
    fn render_stage_tabs_linear_empty_stages() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-s", AgentDisplayStatus::Active);
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 3);
                dash.render_stage_tabs(f, area, &agent);
            })
            .unwrap();
    }

    #[test]
    fn render_stage_tabs_linear_with_records() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-sr", AgentDisplayStatus::Active);
        agent.stages = vec![
            make_stage_record("plan", StageRunStatus::Complete),
            make_stage_record("implement", StageRunStatus::Active),
        ];
        agent.num_stages = 2;
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 3);
                dash.render_stage_tabs(f, area, &agent);
            })
            .unwrap();
    }

    #[test]
    fn draw_linear_tabs_various_statuses() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-vs", AgentDisplayStatus::Active);
        agent.stages = vec![
            make_stage_record("s1", StageRunStatus::Complete),
            make_stage_record("s2", StageRunStatus::WaitingInput),
            make_stage_record("s3", StageRunStatus::Active),
            make_stage_record("s4", StageRunStatus::Pending),
            make_stage_record("s5", StageRunStatus::Error),
        ];
        agent.num_stages = 5;
        agent.stage_index = 2;
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 3);
                dash.draw_linear_tabs(f, area, &agent);
            })
            .unwrap();
    }

    #[test]
    fn build_edge_summary_none() {
        let result = Dashboard::build_edge_summary(None);
        assert!(result.is_empty());
    }

    #[test]
    fn build_edge_summary_empty_edges() {
        let edges: Vec<GraphEdge> = vec![];
        let result = Dashboard::build_edge_summary(Some(&edges));
        assert!(result.contains("terminal"));
    }

    #[test]
    fn build_edge_summary_multiple_edges() {
        let edges = vec![
            GraphEdge {
                target: "stage_b".to_string(),
                hint: Some("on success".to_string()),
                condition: "always".to_string(),
                transform: "replace".to_string(),
            },
            GraphEdge {
                target: "stage_c".to_string(),
                hint: None,
                condition: "always".to_string(),
                transform: "replace".to_string(),
            },
        ];
        let result = Dashboard::build_edge_summary(Some(&edges));
        assert!(result.contains("stage_b"));
        assert!(result.contains("stage_c"));
        assert!(result.contains("on success"));
    }

    #[test]
    fn build_edge_summary_filters_error() {
        let edges = vec![
            GraphEdge {
                target: "error_handler".to_string(),
                hint: None,
                condition: "error".to_string(),
                transform: "replace".to_string(),
            },
            GraphEdge {
                target: "next".to_string(),
                hint: None,
                condition: "always".to_string(),
                transform: "replace".to_string(),
            },
        ];
        let result = Dashboard::build_edge_summary(Some(&edges));
        assert!(!result.contains("error_handler"));
        assert!(result.contains("next"));
    }

    #[test]
    fn render_stage_tabs_graph_mode() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-g", AgentDisplayStatus::Active);
        agent.stages = vec![
            make_stage_record("plan", StageRunStatus::Complete),
            make_stage_record("implement", StageRunStatus::Active),
        ];
        agent.num_stages = 2;
        let mut edges = HashMap::new();
        edges.insert(
            "plan".to_string(),
            vec![GraphEdge {
                target: "implement".to_string(),
                hint: None,
                condition: "always".to_string(),
                transform: "replace".to_string(),
            }],
        );
        agent.graph_info = Some(GraphTransitionInfo {
            edges,
            entry_stage: "plan".to_string(),
            stage_names: vec!["plan".to_string(), "implement".to_string()],
        });
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 7);
                dash.render_stage_tabs(f, area, &agent);
            })
            .unwrap();
    }

    #[test]
    fn draw_graph_view_basic() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-gv", AgentDisplayStatus::Active);
        agent.stage = "plan".to_string();
        agent.stages = vec![make_stage_record("plan", StageRunStatus::Active)];
        let mut edges = HashMap::new();
        edges.insert(
            "plan".to_string(),
            vec![GraphEdge {
                target: "implement".to_string(),
                hint: Some("after planning".to_string()),
                condition: "always".to_string(),
                transform: "replace".to_string(),
            }],
        );
        let graph = GraphTransitionInfo {
            edges,
            entry_stage: "plan".to_string(),
            stage_names: vec!["plan".to_string(), "implement".to_string()],
        };
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 7);
                dash.draw_graph_view(f, area, &agent, &graph);
            })
            .unwrap();
    }

    #[test]
    fn draw_graph_view_bidirectional_edges() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-bi", AgentDisplayStatus::Active);
        agent.stage = "plan".to_string();
        agent.stages = vec![
            make_stage_record("plan", StageRunStatus::Complete),
            make_stage_record("implement", StageRunStatus::Active),
        ];
        let mut edges = HashMap::new();
        edges.insert(
            "plan".to_string(),
            vec![GraphEdge {
                target: "implement".to_string(),
                hint: None,
                condition: "always".to_string(),
                transform: "replace".to_string(),
            }],
        );
        edges.insert(
            "implement".to_string(),
            vec![GraphEdge {
                target: "plan".to_string(),
                hint: None,
                condition: "always".to_string(),
                transform: "replace".to_string(),
            }],
        );
        let graph = GraphTransitionInfo {
            edges,
            entry_stage: "plan".to_string(),
            stage_names: vec!["plan".to_string(), "implement".to_string()],
        };
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 7);
                dash.draw_graph_view(f, area, &agent, &graph);
            })
            .unwrap();
    }

    #[test]
    fn draw_graph_view_stale_active_stage_on_done_run_renders_success_color() {
        // A non-current stage still marked Active in its stage record, on a
        // run whose overall status is Complete, hits the
        // `StageRunStatus::Active if run_done => C_SUCCESS` arm (as opposed
        // to the live `StageRunStatus::Active => C_ACCENT` arm, which only
        // applies while the run is still going).
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-stale-active-done", AgentDisplayStatus::Complete);
        agent.stage = "plan".to_string();
        agent.stages = vec![
            make_stage_record("plan", StageRunStatus::Complete),
            make_stage_record("implement", StageRunStatus::Active),
        ];
        let mut edges = HashMap::new();
        edges.insert(
            "plan".to_string(),
            vec![GraphEdge {
                target: "implement".to_string(),
                hint: None,
                condition: "always".to_string(),
                transform: "replace".to_string(),
            }],
        );
        let graph = GraphTransitionInfo {
            edges,
            entry_stage: "plan".to_string(),
            stage_names: vec!["plan".to_string(), "implement".to_string()],
        };
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 7);
                dash.draw_graph_view(f, area, &agent, &graph);
            })
            .unwrap();
    }

    #[test]
    fn draw_graph_view_no_visible_stages_shows_placeholder() {
        // No stage records, no reachable stages, and an entry_stage that
        // isn't in stage_names at all -> visible_stages ends up empty.
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-empty-graph", AgentDisplayStatus::Active);
        agent.stage = "nowhere".to_string();
        agent.stages = vec![];
        let graph = GraphTransitionInfo {
            edges: HashMap::new(),
            entry_stage: "missing_entry".to_string(),
            stage_names: vec![],
        };
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 7);
                dash.draw_graph_view(f, area, &agent, &graph);
            })
            .unwrap();

        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(content.contains("No stages yet"));
    }

    #[test]
    fn draw_graph_view_shows_entry_stage_even_when_unreached() {
        // "plan" is the entry_stage but current position is "implement" with
        // no path back to "plan" -> plan is shown only via the
        // `**name == graph.entry_stage` fallback, and colored C_DIM since
        // it's not in stage_statuses and not `reachable`.
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-entry-unreached", AgentDisplayStatus::Active);
        agent.stage = "implement".to_string();
        agent.stages = vec![make_stage_record("implement", StageRunStatus::Active)];
        let graph = GraphTransitionInfo {
            edges: HashMap::new(), // no outgoing edges from "implement"
            entry_stage: "plan".to_string(),
            stage_names: vec!["plan".to_string(), "implement".to_string()],
        };
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 7);
                dash.draw_graph_view(f, area, &agent, &graph);
            })
            .unwrap();
    }

    #[test]
    fn draw_graph_view_dangling_edge_targets_hit_cycle_detection() {
        // Two edges pointing at stage names that don't exist in stage_names
        // both resolve to "" via unwrap_or(""), so "" gets queued twice —
        // exercising the `if !set.insert(name) { continue; }` cycle guard
        // when the second "" is popped and already visited.
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-dangling", AgentDisplayStatus::Active);
        agent.stage = "plan".to_string();
        agent.stages = vec![make_stage_record("plan", StageRunStatus::Active)];
        let mut edges = HashMap::new();
        edges.insert(
            "plan".to_string(),
            vec![
                GraphEdge {
                    target: "ghost_one".to_string(),
                    hint: None,
                    condition: "always".to_string(),
                    transform: "replace".to_string(),
                },
                GraphEdge {
                    target: "ghost_two".to_string(),
                    hint: None,
                    condition: "always".to_string(),
                    transform: "replace".to_string(),
                },
            ],
        );
        let graph = GraphTransitionInfo {
            edges,
            entry_stage: "plan".to_string(),
            stage_names: vec!["plan".to_string()], // neither ghost target exists
        };
        // Must not panic/loop forever despite the dangling edge targets.
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 7);
                dash.draw_graph_view(f, area, &agent, &graph);
            })
            .unwrap();
    }

    #[test]
    fn draw_graph_view_multi_visit_count_and_error_glyph() {
        // "plan" was visited twice (vc=2, exercising the "x{vc}" suffix) and
        // its last recorded status is Error while it's not the current
        // stage -> the visited-but-not-current glyph should be GLYPH_ERROR.
        let backend = TestBackend::new(160, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-multivisit", AgentDisplayStatus::Active);
        agent.stage = "implement".to_string();
        agent.stages = vec![
            make_stage_record("plan", StageRunStatus::Complete),
            make_stage_record("plan", StageRunStatus::Error),
            make_stage_record("implement", StageRunStatus::Active),
        ];
        let mut edges = HashMap::new();
        edges.insert(
            "plan".to_string(),
            vec![GraphEdge {
                target: "implement".to_string(),
                hint: None,
                condition: "always".to_string(),
                transform: "replace".to_string(),
            }],
        );
        let graph = GraphTransitionInfo {
            edges,
            entry_stage: "plan".to_string(),
            stage_names: vec!["plan".to_string(), "implement".to_string()],
        };
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 160, 7);
                dash.draw_graph_view(f, area, &agent, &graph);
            })
            .unwrap();

        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(content.contains("x2"));
    }

    #[test]
    fn draw_graph_view_all_status_colors_and_reverse_only_arrow() {
        // Exercises WaitingInput/Pending glyph colors, a reverse-only arrow
        // (b has an edge back to a, but a has none to b), and an
        // unreachable/unvisited stage colored C_DIM.
        let backend = TestBackend::new(200, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-colors", AgentDisplayStatus::Active);
        agent.stage = "a".to_string();
        agent.stages = vec![
            make_stage_record("a", StageRunStatus::Active),
            make_stage_record("b", StageRunStatus::WaitingInput),
            make_stage_record("c", StageRunStatus::Pending),
        ];
        let mut edges = HashMap::new();
        // b -> a (reverse-only relative to a/b pairing; a has no edge to b)
        edges.insert(
            "b".to_string(),
            vec![GraphEdge {
                target: "a".to_string(),
                hint: None,
                condition: "always".to_string(),
                transform: "replace".to_string(),
            }],
        );
        let graph = GraphTransitionInfo {
            edges,
            entry_stage: "a".to_string(),
            stage_names: vec!["a".to_string(), "b".to_string(), "c".to_string()],
        };
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 200, 7);
                dash.draw_graph_view(f, area, &agent, &graph);
            })
            .unwrap();
    }

    #[test]
    fn draw_graph_view_breaks_when_total_width_exceeds_area() {
        // Many stages in a narrow area force the `total_w > inner_w` early
        // break while laying out nodes.
        let backend = TestBackend::new(30, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-narrow", AgentDisplayStatus::Active);
        agent.stage = "s1".to_string();
        let mut edges = HashMap::new();
        let stage_names: Vec<String> = (1..=8).map(|i| format!("s{}", i)).collect();
        agent.stages = stage_names
            .iter()
            .map(|n| make_stage_record(n, StageRunStatus::Complete))
            .collect();
        for w in stage_names.windows(2) {
            edges.insert(
                w[0].clone(),
                vec![GraphEdge {
                    target: w[1].clone(),
                    hint: None,
                    condition: "always".to_string(),
                    transform: "replace".to_string(),
                }],
            );
        }
        let graph = GraphTransitionInfo {
            edges,
            entry_stage: "s1".to_string(),
            stage_names,
        };
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 30, 7);
                dash.draw_graph_view(f, area, &agent, &graph);
            })
            .unwrap();
    }

    #[test]
    fn draw_linear_tabs_fallback_marks_stages_before_current_as_complete() {
        // Empty agent.stages triggers the synthesized-fallback path; a
        // stage_index > 0 exercises the "i < stage_index -> Complete glyph"
        // branch for the earlier, already-passed stages.
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-fallback-idx", AgentDisplayStatus::Active);
        agent.stages = vec![];
        agent.num_stages = 3;
        agent.stage_index = 2;
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 120, 3);
                dash.render_stage_tabs(f, area, &agent);
            })
            .unwrap();
    }

    #[test]
    fn build_stage_tab_title_duration_over_a_minute_shows_minutes() {
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-mins", AgentDisplayStatus::Active);
        let mut record = make_stage_record("plan", StageRunStatus::Complete);
        record.started_at = Some(0);
        record.ended_at = Some(125); // 2m5s
        let line = dash.build_stage_tab_title(0, &record, &agent);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("2m5s"));
    }

    #[test]
    fn build_stage_tab_title_active_current_tab_uses_active_until() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-active-until", AgentDisplayStatus::Active);
        agent.stage_index = 0;
        agent.active_until = Some(chrono::Utc::now().timestamp());
        let mut record = make_stage_record("plan", StageRunStatus::Active);
        record.started_at = Some(chrono::Utc::now().timestamp() - 30);
        record.ended_at = None;
        // is_current_tab (i == agent.stage_index) with an active_until set
        // exercises elapsed_str_until() instead of elapsed_str().
        let line = dash.build_stage_tab_title(0, &record, &agent);
        assert!(!line.spans.is_empty());
    }

    #[test]
    fn build_stage_tab_title_complete() {
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-t", AgentDisplayStatus::Active);
        let record = make_stage_record("plan", StageRunStatus::Complete);
        let line = dash.build_stage_tab_title(0, &record, &agent);
        assert!(!line.spans.is_empty());
    }

    #[test]
    fn build_stage_tab_title_active_running() {
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-t2", AgentDisplayStatus::Active);
        let record = make_stage_record("implement", StageRunStatus::Active);
        let line = dash.build_stage_tab_title(0, &record, &agent);
        assert!(!line.spans.is_empty());
    }

    #[test]
    fn build_stage_tab_title_active_run_done() {
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-t3", AgentDisplayStatus::Complete);
        let record = make_stage_record("implement", StageRunStatus::Active);
        let line = dash.build_stage_tab_title(0, &record, &agent);
        assert!(!line.spans.is_empty());
    }

    // ─── Regression: stale Active stage record on a non-live tab ───────────
    //
    // A stage record can be left stuck at StageRunStatus::Active (e.g. a
    // prior interactive/interactive_points stage whose completion was never
    // recorded) even though the run has moved on to a later stage. Only the
    // tab matching agent.stage_index should ever show the spinner/live
    // marker — every other tab must render as Complete instead.

    #[test]
    fn build_stage_tab_title_stale_active_on_non_current_tab_shows_complete() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-stale", AgentDisplayStatus::Active);
        agent.stage_index = 1; // run has moved on to stage 1 ("implement")
                               // Stage 0 ("plan") record is stuck Active — simulating the bug where
                               // on_stage_result was never called for an Interactive/InteractivePoints
                               // stage.
        let stale_plan = make_stage_record("plan", StageRunStatus::Active);
        let line = dash.build_stage_tab_title(0, &stale_plan, &agent);

        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            rendered.contains(GLYPH_COMPLETE),
            "stale Active stage on a non-current tab must render as Complete, got: {:?}",
            rendered
        );
        for spin in SPINNER.iter() {
            assert!(
                !rendered.contains(spin),
                "stale Active stage on a non-current tab must not show a spinner, got: {:?}",
                rendered
            );
        }
        assert!(
            !rendered.contains('*'),
            "stale Active stage on a non-current tab must not show the live marker, got: {:?}",
            rendered
        );

        // The actually-live tab (index == agent.stage_index) should still spin.
        let live_implement = make_stage_record("implement", StageRunStatus::Active);
        let live_line = dash.build_stage_tab_title(1, &live_implement, &agent);
        let live_rendered: String = live_line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            SPINNER.iter().any(|spin| live_rendered.contains(spin)),
            "the actually-live tab must still show a spinner, got: {:?}",
            live_rendered
        );
    }
}
