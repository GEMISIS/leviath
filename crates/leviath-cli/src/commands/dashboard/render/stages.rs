//! Stage tab bar and graph view rendering.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Tabs};

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
        // Both agent shapes use the same compact tab strip; a graph agent's
        // real DAG (with revisit counts and the visit timeline) lives in the
        // full-screen explorer on `g`, instead of a 7-row strip that could
        // only draw arrows between list-neighbors.
        self.draw_linear_tabs(frame, tabs_area, agent);
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

        let mut tab_nav = if tabs_count > 1 {
            format!(" ←/→ to switch  stage {}/{}", selected_tab + 1, tabs_count)
        } else {
            " stage 1/1".to_string()
        };
        if agent.graph_info.is_some() {
            tab_nav.push_str("  ·  [g] graph");
        }

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
        // recorded) even though the run has moved on - render those as
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::dashboard::graph::{GraphEdge, GraphTransitionInfo};
    use crate::commands::dashboard::test_support::{make_test_dashboard, rendered_buffer};
    use crate::runstate::{StageRecord, StageRunStatus};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use std::collections::HashMap;

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
            last_progress_at: None,
            active_until: None,
            waiting_secs: 0,
            graph_info: None,
            accepts_messages: true,
            taint_summary: vec![],
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("main"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("plan"), "{buf}");
        assert!(buf.contains("implement"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("s1"), "{buf}");
        assert!(buf.contains("s2"), "{buf}");
        assert!(buf.contains("s3"), "{buf}");
        assert!(buf.contains("s4"), "{buf}");
        assert!(buf.contains("s5"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("plan"), "{buf}");
        assert!(buf.contains("implement"), "{buf}");
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
        let buf = rendered_buffer(&terminal);
        assert!(buf.contains("main"), "{buf}");
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
    // marker - every other tab must render as Complete instead.

    #[test]
    fn build_stage_tab_title_stale_active_on_non_current_tab_shows_complete() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-stale", AgentDisplayStatus::Active);
        agent.stage_index = 1; // run has moved on to stage 1 ("implement")
        // Stage 0 ("plan") record is stuck Active - simulating the bug where
        // on_stage_result was never called for an Interactive/InteractivePoints
        // stage.
        let stale_plan = make_stage_record("plan", StageRunStatus::Active);
        let line = dash.build_stage_tab_title(0, &stale_plan, &agent);

        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(rendered.contains(GLYPH_COMPLETE));
        for spin in SPINNER.iter() {
            assert!(!rendered.contains(spin));
        }
        assert!(!rendered.contains('*'));

        // The actually-live tab (index == agent.stage_index) should still spin.
        let live_implement = make_stage_record("implement", StageRunStatus::Active);
        let live_line = dash.build_stage_tab_title(1, &live_implement, &agent);
        let live_rendered: String = live_line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(SPINNER.iter().any(|spin| live_rendered.contains(spin)));
    }
}
