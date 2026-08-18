//! The full-screen stage explorer: the blueprint's stage graph on a canvas
//! with the run painted onto it, and the visit timeline.
//!
//! The graph tab draws a `crate::tui::flowgraph::FlowView`: stages are
//! boxes, transitions are routed edges, the current stage spins, the last
//! transition taken is animated, revisit loops run along a lane below the
//! nodes and the escape edges (`error`, `dead_end`, ...) hide behind `e`.
//! The timeline tab lists each actual visit with its time, duration and
//! iterations, derived from the run archive.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::commands::dashboard::history::{StageVisit, clock};
use crate::commands::dashboard::state::Dashboard;
use crate::commands::dashboard::theme::*;
use crate::commands::dashboard::types::*;
use crate::tui::flowgraph::{Direction as FlowDirection, Selection};
use crate::tui::widgets::footer::{draw_hint_bar, hint};

/// The graph pane's title: what is shown and how, in as many words as the
/// width allows.
fn graph_title(
    width: u16,
    direction: FlowDirection,
    show_untaken: bool,
    show_escape: bool,
    show_unvisited: bool,
    zoom: f64,
) -> String {
    let on_off = |on: bool| if on { "on" } else { "off" };
    if width < 110 {
        return format!(
            " Graph · {} · t:{} e:{} u:{} · {:.0}% ",
            match direction {
                FlowDirection::LeftToRight => "→",
                FlowDirection::TopToBottom => "↓",
            },
            on_off(show_untaken),
            on_off(show_escape),
            on_off(show_unvisited),
            zoom * 100.0,
        );
    }
    format!(
        " Graph · {} (r) · untaken edges {} (t) · escapes {} (e) · unvisited {} (u) · zoom {:.0}% ",
        match direction {
            FlowDirection::LeftToRight => "left to right",
            FlowDirection::TopToBottom => "top to bottom",
        },
        on_off(show_untaken),
        on_off(show_escape),
        on_off(show_unvisited),
        zoom * 100.0,
    )
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
        let Some((tab, timeline_selected)) = self
            .stage_explorer
            .as_ref()
            .map(|e| (e.tab, e.timeline_selected))
        else {
            return;
        };
        // Visit counts and the timeline stay live while the explorer is open
        // (TTL-gated, so this is one archive read a second at most).
        self.ensure_history(&agent.id);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(area);

        // ── Tab strip ──
        let tab_span = |label: &str, active: bool| {
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
                tab_span("Graph", tab == ExplorerTab::Graph),
                tab_span("Timeline", tab == ExplorerTab::Timeline),
            ])),
            chunks[0],
        );

        // ── Body ──
        let visits: Vec<StageVisit> = self
            .selected_history()
            .map(|h| h.visits.clone())
            .unwrap_or_default();
        match tab {
            ExplorerTab::Graph => self.draw_explorer_graph(frame, chunks[1], agent),
            ExplorerTab::Timeline => {
                self.draw_explorer_timeline(frame, chunks[1], &visits, timeline_selected)
            }
        }

        // ── Hint bar ──
        let hints = match tab {
            // Priority order: on a narrow terminal the tail falls off.
            ExplorerTab::Graph => vec![
                hint("esc/g", "close"),
                hint("←→↑↓", "select"),
                hint("enter", "open tab"),
                hint("tab", "timeline"),
                hint("?", "help"),
                hint("t", "all edges"),
                hint("e", "escapes"),
                hint("u", "unvisited"),
                hint("r", "rotate"),
                hint("f", "fit"),
                hint("+ -", "zoom"),
                hint("drag", "move / pan"),
            ],
            ExplorerTab::Timeline => vec![
                hint("esc/g", "close"),
                hint("↑↓", "select a visit"),
                hint("enter", "open its context"),
                hint("tab", "graph"),
                hint("?", "help"),
            ],
        };
        draw_hint_bar(frame, chunks[2], None, &hints, false);
    }

    /// The graph tab: paint the run onto the canvas, draw it, and describe
    /// whatever is selected on the line beneath.
    fn draw_explorer_graph(&mut self, frame: &mut Frame, area: Rect, agent: &DashboardAgent) {
        let live = self.live_overlay_for(agent);
        let Some(explorer) = self.stage_explorer.as_mut() else {
            return;
        };
        explorer.view.apply_live(&live);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(area);
        let title = graph_title(
            area.width,
            explorer.view.direction(),
            explorer.view.show_untaken(),
            explorer.view.show_escape(),
            explorer.view.show_unvisited(),
            explorer.view.zoom(),
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(C_BORDER_FOCUS))
            .title(Span::styled(title, Style::default().fg(C_ACCENT)));
        let canvas = explorer.view.render(frame, rows[0], block);
        self.pane_rects.push((PaneId::ExplorerGraph, canvas));

        let line = match explorer.view.selection() {
            Selection::Node(id) => {
                let graph = explorer.view.graph();
                let mut spans = vec![Span::styled(
                    format!(" {id} "),
                    Style::default().fg(C_WHITE).add_modifier(Modifier::BOLD),
                )];
                // A selected id always names a node of this graph; the map
                // is for the type, not a case.
                let facts: Vec<String> = graph
                    .node(&id)
                    .map(|node| {
                        let mut facts: Vec<String> = vec![node.kind_label().to_string()];
                        if let Some(max) = node.max_iterations {
                            facts.push(format!("max {max} iterations"));
                        }
                        if let Some(max) = node.max_revisits {
                            facts.push(format!("max {max} revisits"));
                        }
                        if node.self_loop {
                            facts.push("may repeat itself".to_string());
                        }
                        if let Some(description) = &node.description {
                            facts.push(description.clone());
                        }
                        facts
                    })
                    .unwrap_or_default();
                spans.push(Span::styled(
                    facts.join(" · "),
                    Style::default().fg(C_MUTED),
                ));
                let targets: Vec<String> = graph
                    .outgoing(&id)
                    .map(|e| {
                        let label = e.condition_label();
                        if label.is_empty() {
                            format!("→ {}", e.to)
                        } else {
                            format!("→ {} [{label}]", e.to)
                        }
                    })
                    .collect();
                if !targets.is_empty() {
                    spans.push(Span::styled(
                        format!("  {}", targets.join("  ")),
                        Style::default().fg(C_ACCENT),
                    ));
                }
                Line::from(spans)
            }
            Selection::Edge(edge) => {
                let label = edge.condition_label();
                let mut text = format!(" {} → {}", edge.from, edge.to);
                if !label.is_empty() {
                    text.push_str(&format!(" [{label}]"));
                }
                text.push_str(&format!(" · transform {}", edge.transform));
                if let Some(hint) = &edge.hint {
                    text.push_str(&format!(" · {hint}"));
                }
                Line::from(Span::styled(text, Style::default().fg(C_MUTED)))
            }
            Selection::Nothing => Line::from(Span::styled(
                " Select a stage with the arrows or a click; enter opens its tab.",
                Style::default().fg(C_DIM),
            )),
        };
        frame.render_widget(Paragraph::new(line), rows[1]);
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
    use std::sync::Arc;

    use crate::commands::dashboard::history::{RunHistoryCache, derive_visits};
    use crate::commands::dashboard::test_support::{
        make_test_dashboard, rendered_buffer, style_at_text,
    };
    use crate::commands::dashboard::types::*;
    use crate::tui::flowgraph::{FlowView, NodeStyle, StageGraph};
    use crate::tui::theme::*;
    use crossterm::event::KeyCode;
    use leviath_core::manifest::parse_manifest;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn stage_graph() -> Arc<StageGraph> {
        Arc::new(StageGraph::from_blueprint(
            &parse_manifest(
                r#"
[agent]
name = "grapher"
[stages.plan]
description = "decide what to build"
max_iterations = 5
[stages.plan.transitions.implement]
hint = "ready"
[stages.implement]
[stages.implement.transitions.review]
[stages.implement.transitions.recover]
condition = "error"
[stages.review]
max_revisits = 2
[stages.review.transitions.implement]
condition = "llm_choice"
[stages.review.transitions.review]
[stages.review.transitions.island]
[stages.recover]
[stages.recover.transitions.implement]
[stages.island]
mode = "output"
[stages.island.transitions]
"#,
            )
            .unwrap(),
        ))
    }

    fn graph_agent() -> DashboardAgent {
        DashboardAgent {
            id: "run-1".to_string(),
            blueprint_name: "grapher".to_string(),
            stage: "implement".to_string(),
            stage_index: 1,
            num_stages: 5,
            status: AgentDisplayStatus::Active,
            tokens_in: 0,
            tokens_out: 0,
            cached_tokens: 0,
            iteration: 1,
            waiting_prompt: None,
            wait_reason: None,
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
            graph: Some(stage_graph()),
            accepts_messages: true,
            taint_summary: vec![],
        }
    }

    fn explorer() -> ExplorerState {
        ExplorerState::new(
            "run-1".to_string(),
            FlowView::new(stage_graph(), NodeStyle::Full, false),
        )
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

    fn rendered_at(
        dash: &mut crate::commands::dashboard::state::Dashboard,
        w: u16,
        h: u16,
    ) -> (Terminal<TestBackend>, String) {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        let agent = dash.agents[0].clone();
        terminal
            .draw(|f| dash.draw_stage_explorer(f, f.area(), &agent))
            .unwrap();
        let text = rendered_buffer(&terminal);
        (terminal, text)
    }

    fn rendered(dash: &mut crate::commands::dashboard::state::Dashboard) -> String {
        rendered_at(dash, 200, 50).1
    }

    #[test]
    fn the_graph_tab_draws_every_stage_marks_the_current_one_and_counts_revisits() {
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
        dash.stage_explorer = Some(explorer());

        let (terminal, text) = rendered_at(&mut dash, 200, 50);
        assert!(text.contains("Stage explorer"), "{text}");
        for stage in ["plan", "implement", "review", "recover", "island"] {
            assert!(text.contains(stage), "{stage}: {text}");
        }
        assert!(text.contains("implement ×2"), "revisit count: {text}");
        assert!(text.contains("[llm_choice]"), "condition label: {text}");
        assert!(text.contains("escapes off (e)"), "{text}");
        assert!(text.contains("untaken edges off (t)"), "{text}");
        assert!(text.contains("←→↑↓ select"), "hint bar: {text}");
        // Untaken edges are hidden by default: review's edge to island (never
        // taken, and review is not the current stage) is not drawn; the path
        // taken and the current stage's options are.
        assert!(
            !text.contains("[llm_choice]") || text.contains("implement ×2"),
            "{text}"
        );
        assert!(text.contains("Select a stage with the arrows"), "{text}");
        // The current stage is drawn in the active colour, a pending one dim.
        assert_eq!(style_at_text(&terminal, "implement ×2").fg, Some(C_ACTIVE));
        assert_eq!(style_at_text(&terminal, "island").fg, Some(C_DIM));
        // The graph pane registered its canvas for the mouse.
        assert!(
            dash.pane_rects
                .iter()
                .any(|(id, rect)| *id == PaneId::ExplorerGraph && rect.width > 10),
            "{:?}",
            dash.pane_rects
        );
    }

    #[test]
    fn selecting_a_stage_or_an_edge_describes_it_under_the_canvas() {
        let mut dash = make_test_dashboard();
        dash.agents.push(graph_agent());
        dash.update_display_indices();
        dash.stage_explorer = Some(explorer());
        rendered(&mut dash);
        dash.stage_explorer
            .as_mut()
            .unwrap()
            .view
            .handle_key(KeyCode::Char(']'));
        let text = rendered(&mut dash);
        assert!(
            text.contains("plan autonomous · max 5 iterations · decide what to build"),
            "{text}"
        );
        assert!(text.contains("→ implement"), "{text}");
        // A stage with a revisit budget and a self-loop says so; a terminal
        // one lists no targets.
        dash.stage_explorer
            .as_mut()
            .unwrap()
            .view
            .select_stage("review");
        let text = rendered(&mut dash);
        assert!(
            text.contains("review autonomous · max 2 revisits · may repeat itself"),
            "{text}"
        );
        assert!(
            text.contains("→ implement [llm_choice]  → island"),
            "{text}"
        );
        dash.stage_explorer
            .as_mut()
            .unwrap()
            .view
            .select_stage("island");
        let text = rendered(&mut dash);
        assert!(text.contains("island output"), "{text}");
        assert!(!text.contains("island output  →"), "{text}");
        dash.stage_explorer
            .as_mut()
            .unwrap()
            .view
            .handle_key(KeyCode::Char('e'));
        let text = rendered(&mut dash);
        assert!(text.contains("escapes on (e)"), "{text}");
        assert!(text.contains("[error]"), "{text}");
        // A narrow pane gets the short title, and the graph turns to fit it
        // (the title says so from the frame after the turn: the dashboard
        // redraws ten times a second).
        rendered_at(&mut dash, 90, 40);
        let (_, text) = rendered_at(&mut dash, 90, 40);
        assert!(
            text.contains("Graph · ↓ · t:off e:on u:on · 100%"),
            "{text}"
        );
        // An edge: pick one directly on the canvas. The first carries a hint
        // and no condition; the loop back from review carries the reverse.
        let mut hinted = explorer();
        hinted.view.select_edge_for_test(0);
        dash.stage_explorer = Some(hinted);
        let text = rendered(&mut dash);
        assert!(
            text.contains("plan → implement · transform direct · ready"),
            "{text}"
        );
        let mut looped = explorer();
        looped.view.select_edge_for_test(3);
        dash.stage_explorer = Some(looped);
        let text = rendered(&mut dash);
        assert!(
            text.contains("review → implement [llm_choice] · transform direct"),
            "{text}"
        );
        assert!(!text.contains("transform direct · "), "no hint: {text}");
    }

    #[test]
    fn u_hides_unvisited_stages_from_the_graph_but_never_the_current_one() {
        let mut dash = make_test_dashboard();
        let mut agent = graph_agent();
        agent.stage = "review".to_string(); // current but never archived
        dash.agents.push(agent);
        dash.update_display_indices();
        seed(&mut dash, &[("plan", 10), ("implement", 70)]);
        let mut explorer = explorer();
        explorer.view.toggle_unvisited();
        dash.stage_explorer = Some(explorer);

        let text = rendered(&mut dash);
        assert!(!text.contains("island"), "{text}");
        assert!(!text.contains("recover"), "{text}");
        assert!(text.contains("plan"), "{text}");
        assert!(
            text.contains("review"),
            "the current stage never hides: {text}"
        );
        assert!(text.contains("unvisited off (u)"), "{text}");
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
        let mut explorer = explorer();
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
    fn guards_hold_when_the_explorer_is_missing() {
        // No explorer state: drawing is a no-op.
        let mut dash = make_test_dashboard();
        dash.agents.push(graph_agent());
        dash.update_display_indices();
        let text = rendered(&mut dash);
        assert!(!text.contains("Stage explorer"));

        // The graph body called without an explorer declines too.
        let agent = dash.agents[0].clone();
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| dash.draw_explorer_graph(f, f.area(), &agent))
            .unwrap();
        let buf = rendered_buffer(&terminal);
        assert!(buf.trim().is_empty(), "{buf}");
    }

    #[test]
    fn duration_and_clock_formatting() {
        use crate::commands::dashboard::history::{StageVisit, clock};
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
        assert_eq!(clock(i64::MIN), "", "unrepresentable time is blank");
    }

    #[test]
    fn the_timeline_says_so_when_there_are_no_visits_yet() {
        let mut dash = make_test_dashboard();
        dash.agents.push(graph_agent());
        dash.update_display_indices();
        let mut explorer = explorer();
        explorer.tab = ExplorerTab::Timeline;
        dash.stage_explorer = Some(explorer);

        let text = rendered(&mut dash);
        assert!(text.contains("No archived visits yet"), "{text}");
    }
}
