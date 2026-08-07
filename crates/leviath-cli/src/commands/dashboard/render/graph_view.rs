//! The full-screen stage explorer: a layered rendering of the stage graph,
//! and the visit timeline.
//!
//! The old inline strip drew stages on one row and could only draw arrows
//! between list-neighbors, so any real graph read as a line with missing
//! edges. Here stages are laid out on layers (parallel branches share a
//! layer), every edge is shown under its source - back-edges (revisit loops)
//! distinctly - and the timeline tab lists each actual visit with its time,
//! duration, and iterations, derived from the run archive.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};

use crate::commands::dashboard::graph_layout;
use crate::commands::dashboard::history::{StageVisit, last_visit, visit_count};
use crate::commands::dashboard::state::Dashboard;
use crate::commands::dashboard::theme::*;
use crate::commands::dashboard::types::*;

/// `HH:MM:SS` in local time.
fn clock(at: i64) -> String {
    chrono::DateTime::from_timestamp(at, 0)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}

fn duration_label(visit: &StageVisit) -> String {
    match visit.left_at {
        Some(left) => {
            let secs = (left - visit.entered_at).max(0);
            if secs < 60 {
                format!("{secs}s")
            } else {
                format!("{}m{}s", secs / 60, secs % 60)
            }
        }
        None => "…".to_string(),
    }
}

impl Dashboard {
    /// Draw the explorer over the whole detail area.
    pub(in crate::commands::dashboard) fn draw_stage_explorer(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        agent: &DashboardAgent,
    ) {
        let Some(explorer) = self.stage_explorer.clone() else {
            return;
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(area);

        // ── Tab strip ──
        let tab = |label: &str, active: bool| {
            Span::styled(
                format!(" {label} "),
                if active {
                    Style::default()
                        .fg(C_ACCENT)
                        .add_modifier(Modifier::BOLD | Modifier::REVERSED)
                } else {
                    Style::default().fg(C_MUTED)
                },
            )
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(" Stage explorer · {} ", agent.blueprint_name),
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                ),
                tab("Graph", explorer.tab == ExplorerTab::Graph),
                tab("Timeline", explorer.tab == ExplorerTab::Timeline),
            ])),
            chunks[0],
        );

        // ── Body ──
        let visits: Vec<StageVisit> = self
            .selected_history()
            .map(|h| h.visits.clone())
            .unwrap_or_default();
        match explorer.tab {
            ExplorerTab::Graph => self.draw_explorer_graph(frame, chunks[1], agent, &visits),
            ExplorerTab::Timeline => {
                self.draw_explorer_timeline(frame, chunks[1], &visits, explorer.timeline_selected)
            }
        }

        // ── Hint bar ──
        use crate::tui::widgets::footer::{draw_hint_bar, hint};
        let hints = match explorer.tab {
            ExplorerTab::Graph => vec![
                hint("tab", "timeline"),
                hint("↑↓", "scroll"),
                hint("u", "toggle unvisited"),
                hint("esc/g", "close"),
            ],
            ExplorerTab::Timeline => vec![
                hint("tab", "graph"),
                hint("↑↓", "select a visit"),
                hint("enter", "open its context"),
                hint("esc/g", "close"),
            ],
        };
        draw_hint_bar(frame, chunks[2], None, &hints, false);
    }

    fn draw_explorer_graph(
        &self,
        frame: &mut Frame,
        area: Rect,
        agent: &DashboardAgent,
        visits: &[StageVisit],
    ) {
        let Some(explorer) = self.stage_explorer.as_ref() else {
            return;
        };
        let Some(graph) = agent.graph_info.as_ref() else {
            return;
        };
        let layout = graph_layout::layout(graph);

        // With unvisited stages hidden, their edges hide too: an arrow into
        // a node that is not on screen only confuses.
        let hidden = |name: &str| -> bool {
            !explorer.show_unvisited
                && visit_count(visits, name) == 0
                && !(name == agent.stage
                    && !matches!(
                        agent.status,
                        AgentDisplayStatus::Complete
                            | AgentDisplayStatus::CompleteInteractive
                            | AgentDisplayStatus::Cancelled
                    ))
        };

        let mut lines: Vec<Line<'static>> = Vec::new();
        for l in 0..=layout.max_layer {
            let nodes = layout.layer_nodes(l);
            if nodes.is_empty() {
                continue;
            }
            // The layer's node boxes, side by side. Parallel branches share
            // the row - the thing the one-row strip could not show.
            let mut spans: Vec<Span<'static>> = vec![Span::styled(
                format!("{l:>2}  "),
                Style::default().fg(C_DIM),
            )];
            let mut drew_any = false;
            for node in &nodes {
                let visited = visit_count(visits, &node.name);
                let is_current = node.name == agent.stage
                    && !matches!(
                        agent.status,
                        AgentDisplayStatus::Complete
                            | AgentDisplayStatus::CompleteInteractive
                            | AgentDisplayStatus::Cancelled
                    );
                if hidden(&node.name) {
                    continue;
                }
                drew_any = true;
                let (glyph, style) = if is_current {
                    (
                        GLYPH_ACTIVE,
                        Style::default()
                            .fg(C_ACCENT)
                            .add_modifier(Modifier::BOLD | Modifier::REVERSED),
                    )
                } else if visited > 0 {
                    (GLYPH_COMPLETE, Style::default().fg(C_WHITE))
                } else {
                    (GLYPH_PENDING, Style::default().fg(C_DIM))
                };
                let mut label = format!("[ {glyph} {}", node.name);
                if visited > 1 {
                    label.push_str(&format!(" ×{visited}"));
                }
                if let Some(v) = last_visit(visits, &node.name) {
                    label.push_str(&format!(" · {}", clock(v.entered_at)));
                }
                label.push_str(" ]");
                spans.push(Span::styled(label, style));
                spans.push(Span::raw("   "));
            }
            if !drew_any {
                continue;
            }
            lines.push(Line::from(spans));

            // Every edge leaving this layer, under its source. Forward edges
            // point down; back-edges (revisit loops) are marked distinctly.
            for node in &nodes {
                let edges = layout
                    .edges
                    .iter()
                    .filter(|e| e.from == node.name && !hidden(&e.from) && !hidden(&e.to));
                for edge in edges {
                    let cond = if edge.condition == "always" {
                        String::new()
                    } else {
                        format!(" [{}]", edge.condition)
                    };
                    let hint_part = edge
                        .hint
                        .as_deref()
                        .map(|h| format!(" - {h}"))
                        .unwrap_or_default();
                    if edge.back_edge {
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("      ↺ {} ╌╌▶ {}", edge.from, edge.to),
                                Style::default().fg(C_WARN),
                            ),
                            Span::styled(cond, Style::default().fg(C_WARN)),
                            Span::styled(hint_part, Style::default().fg(C_DIM)),
                        ]));
                    } else {
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("      {} ──▶ {}", edge.from, edge.to),
                                Style::default().fg(C_MUTED),
                            ),
                            Span::styled(cond, Style::default().fg(C_ACCENT)),
                            Span::styled(hint_part, Style::default().fg(C_DIM)),
                        ]));
                    }
                }
            }
            lines.push(Line::from(""));
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(C_BORDER_FOCUS))
            .title(Span::styled(
                " Graph  (layers run top to bottom; ↺ = revisit loop) ",
                Style::default().fg(C_ACCENT),
            ));
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((explorer.scroll.min(u16::MAX as usize) as u16, 0))
                .block(block),
            area,
        );
    }

    fn draw_explorer_timeline(
        &self,
        frame: &mut Frame,
        area: Rect,
        visits: &[StageVisit],
        selected: usize,
    ) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        if visits.is_empty() {
            lines.push(Line::from(Span::styled(
                " No archived visits yet. The timeline fills in as the run records progress.",
                Style::default().fg(C_DIM),
            )));
        }
        for (i, visit) in visits.iter().enumerate() {
            let on_cursor = i == selected;
            let marker = if self
                .context_history_idx
                .is_some_and(|idx| idx == visit.first_point)
            {
                "⏪ "
            } else {
                "   "
            };
            let style = if on_cursor {
                Style::default()
                    .fg(C_WHITE)
                    .add_modifier(Modifier::REVERSED)
            } else {
                Style::default().fg(C_WHITE)
            };
            lines.push(Line::from(vec![
                Span::styled(marker.to_string(), Style::default().fg(C_ACCENT)),
                Span::styled(format!("{:>3}  ", i + 1), Style::default().fg(C_DIM)),
                Span::styled(format!("{:<18}", visit.stage), style),
                Span::styled(
                    format!(
                        "entered {}  ·  {}  ·  iter {}",
                        clock(visit.entered_at),
                        duration_label(visit),
                        visit.iterations
                    ),
                    Style::default().fg(C_MUTED),
                ),
            ]));
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(C_BORDER_FOCUS))
            .title(Span::styled(
                format!(" Timeline · {} visits ", visits.len()),
                Style::default().fg(C_ACCENT),
            ));
        // Keep the selection visible: scroll so it sits inside the window.
        let inner_h = area.height.saturating_sub(2) as usize;
        let scroll = selected.saturating_sub(inner_h.saturating_sub(1)) as u16;
        frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)).block(block), area);
    }
}

#[cfg(test)]
mod tests {
    use crate::commands::dashboard::graph::{GraphEdge, GraphTransitionInfo};
    use crate::commands::dashboard::history::{RunHistoryCache, derive_visits};
    use crate::commands::dashboard::test_support::{make_test_dashboard, rendered_buffer};
    use crate::commands::dashboard::types::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn graph_agent() -> DashboardAgent {
        let mut agent = DashboardAgent {
            id: "run-1".to_string(),
            blueprint_name: "grapher".to_string(),
            stage: "implement".to_string(),
            stage_index: 1,
            num_stages: 3,
            status: AgentDisplayStatus::Active,
            tokens_in: 0,
            tokens_out: 0,
            cached_tokens: 0,
            iteration: 1,
            waiting_prompt: None,
            pending_request: None,
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
            workdir: "/tmp".to_string(),
            task: "t".to_string(),
            title: None,
            model: None,
            parent_id: None,
            depth: 0,
            started_at: 1000,
            last_progress_at: None,
            active_until: None,
            waiting_secs: 0,
            graph_info: None,
            accepts_messages: true,
            taint_summary: vec![],
        };
        let mut edges = std::collections::HashMap::new();
        edges.insert(
            "plan".to_string(),
            vec![GraphEdge {
                target: "implement".to_string(),
                hint: Some("ready".to_string()),
                condition: "always".to_string(),
                transform: "direct".to_string(),
            }],
        );
        edges.insert(
            "implement".to_string(),
            vec![GraphEdge {
                target: "review".to_string(),
                hint: None,
                condition: "always".to_string(),
                transform: "direct".to_string(),
            }],
        );
        edges.insert(
            "review".to_string(),
            vec![GraphEdge {
                target: "implement".to_string(),
                hint: None,
                condition: "llm_choice".to_string(),
                transform: "direct".to_string(),
            }],
        );
        agent.graph_info = Some(GraphTransitionInfo {
            edges,
            entry_stage: "plan".to_string(),
            stage_names: vec![
                "plan".to_string(),
                "implement".to_string(),
                "review".to_string(),
                "island".to_string(),
            ],
        });
        agent
    }

    fn seed(dash: &mut crate::commands::dashboard::state::Dashboard, stages: &[(&str, i64)]) {
        let points: Vec<leviath_core::run_archive::RunPoint> = stages
            .iter()
            .map(|(stage, at)| {
                let mut meta = leviath_core::run_meta::RunMeta::new(
                    "run-1".to_string(),
                    "a".to_string(),
                    "/p".to_string(),
                    "t".to_string(),
                    None,
                    "/w".to_string(),
                    3,
                );
                meta.current_stage = stage.to_string();
                meta.iteration = 2;
                leviath_core::run_archive::RunPoint {
                    meta,
                    context: leviath_core::run_meta::ContextSnapshot {
                        stage_name: stage.to_string(),
                        total_tokens: 0,
                        max_tokens: 100,
                        regions: vec![],
                    },
                    at: *at,
                }
            })
            .collect();
        dash.history = Some(RunHistoryCache {
            run_id: "run-1".to_string(),
            visits: derive_visits(&points),
            points,
            loaded_at_tick: u64::MAX,
        });
    }

    fn rendered(dash: &mut crate::commands::dashboard::state::Dashboard) -> String {
        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let agent = dash.agents[0].clone();
        terminal
            .draw(|f| dash.draw_stage_explorer(f, f.area(), &agent))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn the_graph_tab_shows_layers_edges_revisits_and_back_edges() {
        let mut dash = make_test_dashboard();
        dash.agents.push(graph_agent());
        dash.update_display_indices();
        seed(
            &mut dash,
            &[
                ("plan", 10),
                ("implement", 70),
                ("review", 130),
                ("implement", 200),
            ],
        );
        dash.stage_explorer = Some(ExplorerState::new());

        let text = rendered(&mut dash);
        assert!(text.contains("Stage explorer"), "{text}");
        assert!(text.contains("implement ×2"), "revisit count: {text}");
        assert!(text.contains("plan ──▶ implement"), "forward edge: {text}");
        assert!(
            text.contains("↺ review ╌╌▶ implement"),
            "back edge marked: {text}"
        );
        assert!(text.contains("[llm_choice]"), "condition label: {text}");
        assert!(text.contains("- ready"), "edge hint: {text}");
        assert!(
            text.contains("island"),
            "unreachable stage still shown: {text}"
        );
        assert!(text.contains("toggle unvisited"), "hint bar: {text}");
    }

    #[test]
    fn u_hides_unvisited_stages_from_the_graph() {
        let mut dash = make_test_dashboard();
        dash.agents.push(graph_agent());
        dash.update_display_indices();
        seed(&mut dash, &[("plan", 10), ("implement", 70)]);
        let mut explorer = ExplorerState::new();
        explorer.show_unvisited = false;
        dash.stage_explorer = Some(explorer);

        let text = rendered(&mut dash);
        assert!(!text.contains("island"), "{text}");
        assert!(!text.contains("review"), "unvisited stage hidden: {text}");
        assert!(text.contains("plan"), "{text}");
    }

    #[test]
    fn the_timeline_lists_visits_and_marks_the_browsed_point() {
        let mut dash = make_test_dashboard();
        dash.agents.push(graph_agent());
        dash.update_display_indices();
        seed(
            &mut dash,
            &[("plan", 10), ("implement", 70), ("implement", 100)],
        );
        dash.context_history_idx = Some(1);
        let mut explorer = ExplorerState::new();
        explorer.tab = ExplorerTab::Timeline;
        explorer.timeline_selected = 1;
        dash.stage_explorer = Some(explorer);

        let text = rendered(&mut dash);
        assert!(text.contains("Timeline · 2 visits"), "{text}");
        assert!(text.contains("iter 2"), "{text}");
        assert!(text.contains("⏪"), "browsed point marked: {text}");
        assert!(text.contains("…"), "open-ended visit duration: {text}");
        assert!(text.contains("open its context"), "hint bar: {text}");
    }

    #[test]
    fn guards_hold_when_the_explorer_or_graph_is_missing() {
        // No explorer state: drawing is a no-op.
        let mut dash = make_test_dashboard();
        dash.agents.push(graph_agent());
        dash.update_display_indices();
        let text = rendered(&mut dash);
        assert!(!text.contains("Stage explorer"));

        // Explorer open on a linear agent: the graph body declines.
        let mut dash = make_test_dashboard();
        let mut linear = graph_agent();
        linear.graph_info = None;
        dash.agents.push(linear);
        dash.update_display_indices();
        dash.stage_explorer = Some(ExplorerState::new());
        let text = rendered(&mut dash);
        assert!(text.contains("Stage explorer"), "chrome still draws");
    }

    #[test]
    fn the_graph_body_guard_holds_when_called_without_an_explorer() {
        let mut dash = make_test_dashboard();
        dash.agents.push(graph_agent());
        dash.update_display_indices();
        let agent = dash.agents[0].clone();
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| dash.draw_explorer_graph(f, f.area(), &agent, &[]))
            .unwrap();
        // The guard is the point: with no explorer state there is nothing to
        // lay out, so it must return before drawing rather than draw an empty
        // graph frame.
        let buf = rendered_buffer(&terminal);
        assert!(buf.trim().is_empty(), "{buf}");
    }

    #[test]
    fn the_current_stage_stays_visible_even_unvisited_and_hidden_mode() {
        let mut dash = make_test_dashboard();
        let mut agent = graph_agent();
        agent.stage = "review".to_string(); // current but never archived
        dash.agents.clear();
        dash.agents.push(agent);
        dash.update_display_indices();
        let mut explorer = ExplorerState::new();
        explorer.show_unvisited = false;
        dash.stage_explorer = Some(explorer);
        let text = rendered(&mut dash);
        assert!(
            text.contains("review"),
            "the current stage never hides: {text}"
        );
        assert!(!text.contains("island"), "{text}");
    }

    #[test]
    fn a_graph_whose_entry_is_missing_still_draws_its_stages() {
        // The layout gives such a graph an empty layer 0; the renderer skips
        // it rather than drawing a blank band.
        let mut dash = make_test_dashboard();
        let mut agent = graph_agent();
        agent
            .graph_info
            .as_mut()
            .expect("graph_agent always carries graph info")
            .entry_stage = "ghost".to_string();
        dash.agents.clear();
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.stage_explorer = Some(ExplorerState::new());
        let text = rendered(&mut dash);
        assert!(text.contains("plan"), "{text}");
    }

    #[test]
    fn duration_and_clock_formatting() {
        use crate::commands::dashboard::history::StageVisit;
        let closed = StageVisit {
            stage: "s".to_string(),
            entered_at: 10,
            left_at: Some(95),
            iterations: 1,
            first_point: 0,
        };
        assert_eq!(super::duration_label(&closed), "1m25s");
        let quick = StageVisit {
            left_at: Some(20),
            ..closed.clone()
        };
        assert_eq!(super::duration_label(&quick), "10s");
        let open = StageVisit {
            left_at: None,
            ..closed
        };
        assert_eq!(super::duration_label(&open), "…");
        assert_eq!(super::clock(i64::MIN), "", "unrepresentable time is blank");
    }

    #[test]
    fn the_timeline_says_so_when_there_are_no_visits_yet() {
        let mut dash = make_test_dashboard();
        dash.agents.push(graph_agent());
        dash.update_display_indices();
        let mut explorer = ExplorerState::new();
        explorer.tab = ExplorerTab::Timeline;
        dash.stage_explorer = Some(explorer);

        let text = rendered(&mut dash);
        assert!(text.contains("No archived visits yet"), "{text}");
    }
}
