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

/// The most a stage name may take on the flat strip, and the least it gets.
const STAGE_LABEL_MAX_W: usize = 32;
const STAGE_LABEL_MIN_W: usize = 10;
/// What a tab costs besides its name: the status glyph and its space, the
/// live marker, a duration such as ` 12m 34s`, the `Tabs` padding and the
/// divider to the next tab.
const STAGE_TAB_OVERHEAD: usize = 16;

/// How many columns each stage name may take on a strip whose inside is
/// `inner_width` wide and holds `tabs` tabs. Wide strips show a long name
/// whole; a crowded one falls back to `STAGE_LABEL_MIN_W`, which the strip
/// then clips.
fn stage_label_width(inner_width: u16, tabs: usize) -> usize {
    let per_tab = usize::from(inner_width) / tabs.max(1);
    per_tab
        .saturating_sub(STAGE_TAB_OVERHEAD)
        .clamp(STAGE_LABEL_MIN_W, STAGE_LABEL_MAX_W)
}

impl Dashboard {
    pub(in crate::commands::dashboard) fn render_stage_tabs(
        &mut self,
        frame: &mut Frame,
        tabs_area: Rect,
        agent: &DashboardAgent,
    ) {
        // Given the rows for it (`stage_row_height`), the stage row is the
        // graph band: the same "N of M" and selection, plus where the run
        // has been and how the stages connect. The flat strip is the
        // fallback for short terminals and runs without a readable
        // blueprint; the full-screen explorer stays on `g`.
        if tabs_area.height >= super::super::detail_band::BAND_HEIGHT
            && self.draw_stage_band(frame, tabs_area, agent)
        {
            return;
        }
        self.draw_linear_tabs(frame, tabs_area, agent);
    }

    fn draw_linear_tabs(&mut self, frame: &mut Frame, tabs_area: Rect, agent: &DashboardAgent) {
        let tab_count = if agent.stages.is_empty() {
            agent.num_stages.max(1)
        } else {
            agent.stages.len()
        };
        let label_w = stage_label_width(tabs_area.width.saturating_sub(2), tab_count);
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
                        truncate(&agent.stage, label_w)
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
                .map(|(i, s)| self.build_stage_tab_title(i, s, agent, label_w))
                .collect()
        };

        let tabs_count = tab_titles.len().max(1);
        let selected_tab = self.selected_stage.min(tabs_count - 1);

        let mut tab_nav = if tabs_count > 1 {
            format!(" ←/→ to switch  stage {}/{}", selected_tab + 1, tabs_count)
        } else {
            " stage 1/1".to_string()
        };
        if agent.graph.is_some() {
            tab_nav.push_str("  ·  [g] graph");
        }

        // Where each tab lands, so clicking one opens it. `Tabs` lays them out
        // as (one space, title, one space) joined by the divider, from the
        // inside edge of the block - the same walk it does when it renders.
        let mut column = tabs_area.x.saturating_add(1);
        let right = tabs_area.x.saturating_add(tabs_area.width);
        for (i, title) in tab_titles.iter().enumerate() {
            let width = title.width() as u16 + 2;
            if column >= right {
                break;
            }
            self.register_click(
                Rect::new(
                    column,
                    tabs_area.y.saturating_add(1),
                    width.min(right - column),
                    1,
                ),
                ClickTarget::StageTab(i),
            );
            column = column.saturating_add(width).saturating_add(3);
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
        label_w: usize,
    ) -> Line<'static> {
        use leviath_core::duration::precise;

        // Only the tab matching the agent's current stage index may show live
        // (spinner / ticking-duration) treatment. A stage record can be left
        // stuck at `Active` (e.g. a prior stage whose completion was never
        // recorded) even though the run has moved on - render those as
        // Complete instead of animating a spinner on a stage that isn't
        // actually running.
        let is_current_tab = i == agent.stage_index;

        // How long the stage has been working, on the same clock as the run's
        // own: a stage the run is paused in is still the cursor stage, and
        // measuring it wall-clock counted the pause as time it spent.
        let dur_str = match s.started_at.is_some() {
            true if s.ended_at.is_some()
                || (s.status == StageRunStatus::Active && is_current_tab) =>
            {
                format!(" {}", precise(s.active_runtime_secs(agent.clock_now)))
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
                            truncate(&s.name, label_w),
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
            // A branch the run finished without taking. Drawn like a pending
            // stage rather than a completed one, because that is what it is:
            // the pending glyph on a finished run reads as "never reached",
            // and the complete glyph read as "ran and did nothing".
            StageRunStatus::Skipped => (GLYPH_PENDING, Style::default().fg(C_DIM)),
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
            Span::styled(truncate(&s.name, label_w), label_style),
            Span::styled(dur_str, Style::default().fg(C_DIM)),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::dashboard::test_support::{make_test_dashboard, rendered_buffer};
    use crate::runstate::{StageRecord, StageRunStatus};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

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
            model: None,
            parent_id: None,
            started_at: chrono::Utc::now().timestamp() - 60,
            last_progress_at: None,
            runtime_secs: 0,
            clock_now: 0,
            graph: None,
            accepts_messages: true,
        }
    }

    fn make_stage_record(name: &str, status: StageRunStatus) -> StageRecord {
        StageRecord {
            entered: !matches!(status, StageRunStatus::Pending | StageRunStatus::Skipped),
            status: status.clone(),
            prompt_tokens: 100,
            completion_tokens: 50,
            started_at: Some(chrono::Utc::now().timestamp() - 30),
            ended_at: if status == StageRunStatus::Complete {
                Some(chrono::Utc::now().timestamp())
            } else {
                None
            },
            ..StageRecord::new(name.to_string(), 0)
        }
    }

    #[test]
    fn render_stage_tabs_linear_empty_stages() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
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
        let mut dash = make_test_dashboard();
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
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-vs", AgentDisplayStatus::Active);
        agent.stages = vec![
            make_stage_record("s1", StageRunStatus::Complete),
            make_stage_record("s2", StageRunStatus::WaitingInput),
            make_stage_record("s3", StageRunStatus::Active),
            make_stage_record("s4", StageRunStatus::Pending),
            make_stage_record("s5", StageRunStatus::Error),
            // A branch the run finished without taking. Drawn like a pending
            // stage, since that is what "never reached" looks like.
            make_stage_record("s6", StageRunStatus::Skipped),
        ];
        agent.num_stages = 6;
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

    /// Each tab is registered over the columns it was drawn on, and a strip
    /// too narrow to hold them all stops registering rather than putting
    /// buttons past the edge.
    #[test]
    fn the_stage_tabs_register_where_they_were_drawn() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-tabs", AgentDisplayStatus::Active);
        agent.stages = vec![
            make_stage_record("plan", StageRunStatus::Complete),
            make_stage_record("implement", StageRunStatus::Active),
            make_stage_record("review", StageRunStatus::Pending),
        ];
        agent.num_stages = 3;

        let mut terminal = Terminal::new(TestBackend::new(120, 10)).unwrap();
        terminal
            .draw(|f| dash.draw_linear_tabs(f, Rect::new(0, 0, 120, 3), &agent))
            .unwrap();
        let tabs: Vec<_> = dash
            .click_targets
            .iter()
            .filter(|(_, t)| matches!(t, ClickTarget::StageTab(_)))
            .collect();
        assert_eq!(tabs.len(), 3, "one target per tab");
        // "plan" is drawn inside the first tab's rect.
        let first = tabs[0].0;
        let buf = rendered_buffer(&terminal);
        let width = 120usize;
        let row: String = buf
            .chars()
            .skip(width * (first.y as usize))
            .take(width)
            .collect();
        let at = row.find("plan").expect("the first tab is drawn");
        assert!(
            (first.x as usize..(first.x + first.width) as usize).contains(&at),
            "the rect covers the tab's text: {first:?} vs column {at}"
        );

        // Narrow enough that the later tabs never make it onto the strip.
        dash.click_targets.clear();
        let mut terminal = Terminal::new(TestBackend::new(14, 10)).unwrap();
        terminal
            .draw(|f| dash.draw_linear_tabs(f, Rect::new(0, 0, 14, 3), &agent))
            .unwrap();
        let narrow = dash
            .click_targets
            .iter()
            .filter(|(_, t)| matches!(t, ClickTarget::StageTab(_)))
            .count();
        assert!(narrow < 3, "the strip ran out of room: {narrow} tabs");
    }

    #[test]
    fn render_stage_tabs_graph_mode() {
        let backend = TestBackend::new(120, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-g", AgentDisplayStatus::Active);
        agent.stages = vec![
            make_stage_record("plan", StageRunStatus::Complete),
            make_stage_record("implement", StageRunStatus::Active),
        ];
        agent.num_stages = 2;
        agent.graph = Some(std::sync::Arc::new(
            crate::tui::flowgraph::StageGraph::from_blueprint(
                &leviath_core::manifest::parse_manifest(
                    "[agent]\nname = \"g\"\n[stages.plan]\n[stages.plan.transitions.implement]\n[stages.implement]\n",
                )
                .unwrap(),
            ),
        ));
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
        let mut dash = make_test_dashboard();
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
        let line = dash.build_stage_tab_title(0, &record, &agent, 10);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("2m5s"));
    }

    /// The live stage tab shows the stage's own working time, so a stage the run
    /// is parked in stops counting rather than tracking the wall clock.
    #[test]
    fn build_stage_tab_title_active_current_tab_reads_the_stage_clock() {
        let dash = make_test_dashboard();
        let mut agent = make_test_agent("run-active-until", AgentDisplayStatus::Active);
        agent.stage_index = 0;
        agent.clock_now = 10_000;
        let mut record = make_stage_record("plan", StageRunStatus::Active);
        // Entered an hour ago on the wall clock; 40 seconds of that was work.
        record.started_at = Some(6_400);
        record.ended_at = None;
        record.active = Some(leviath_core::run_meta::ActiveClock {
            banked_secs: 40,
            since: None,
        });
        let line = dash.build_stage_tab_title(0, &record, &agent, 10);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("40s"), "{text}");
    }

    #[test]
    fn build_stage_tab_title_complete() {
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-t", AgentDisplayStatus::Active);
        let record = make_stage_record("plan", StageRunStatus::Complete);
        let line = dash.build_stage_tab_title(0, &record, &agent, 10);
        assert!(!line.spans.is_empty());
    }

    #[test]
    fn build_stage_tab_title_active_running() {
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-t2", AgentDisplayStatus::Active);
        let record = make_stage_record("implement", StageRunStatus::Active);
        let line = dash.build_stage_tab_title(0, &record, &agent, 10);
        assert!(!line.spans.is_empty());
    }

    #[test]
    fn build_stage_tab_title_active_run_done() {
        let dash = make_test_dashboard();
        let agent = make_test_agent("run-t3", AgentDisplayStatus::Complete);
        let record = make_stage_record("implement", StageRunStatus::Active);
        let line = dash.build_stage_tab_title(0, &record, &agent, 10);
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
        let line = dash.build_stage_tab_title(0, &stale_plan, &agent, 10);

        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(rendered.contains(GLYPH_COMPLETE));
        for spin in SPINNER.iter() {
            assert!(!rendered.contains(spin));
        }
        assert!(!rendered.contains('*'));

        // The actually-live tab (index == agent.stage_index) should still spin.
        let live_implement = make_stage_record("implement", StageRunStatus::Active);
        let live_line = dash.build_stage_tab_title(1, &live_implement, &agent, 10);
        let live_rendered: String = live_line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(SPINNER.iter().any(|spin| live_rendered.contains(spin)));
    }

    /// A wide strip shows a long stage name whole; a crowded one falls back
    /// to the minimum width rather than to nothing.
    #[test]
    fn a_stage_name_gets_the_room_the_strip_has() {
        // 118 inside columns across three tabs: 39 each, 23 for the name.
        assert_eq!(stage_label_width(118, 3), 23);
        // Never below the minimum, never above the cap.
        assert_eq!(stage_label_width(30, 6), STAGE_LABEL_MIN_W);
        assert_eq!(stage_label_width(0, 0), STAGE_LABEL_MIN_W);
        assert_eq!(stage_label_width(400, 1), STAGE_LABEL_MAX_W);
    }

    #[test]
    fn a_long_stage_name_is_whole_on_a_wide_strip_and_cut_on_a_narrow_one() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-long", AgentDisplayStatus::Active);
        let long = "gather-the-evidence-carefully";
        agent.stages = vec![
            make_stage_record("plan", StageRunStatus::Complete),
            make_stage_record(long, StageRunStatus::Active),
        ];
        agent.num_stages = 2;
        agent.stage_index = 1;
        agent.stage = long.to_string();

        let mut terminal = Terminal::new(TestBackend::new(120, 10)).unwrap();
        terminal
            .draw(|f| dash.draw_linear_tabs(f, Rect::new(0, 0, 120, 3), &agent))
            .unwrap();
        let wide = rendered_buffer(&terminal);
        assert!(wide.contains(long), "the name fits at 120 columns: {wide}");

        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        terminal
            .draw(|f| dash.draw_linear_tabs(f, Rect::new(0, 0, 40, 3), &agent))
            .unwrap();
        let narrow = rendered_buffer(&terminal);
        assert!(!narrow.contains(long), "{narrow}");
        assert!(narrow.contains("gather-th"), "cut, not dropped: {narrow}");

        // The fallback strip, for a run with no stage records, takes the
        // same width for the live stage's name.
        agent.stages.clear();
        let mut terminal = Terminal::new(TestBackend::new(120, 10)).unwrap();
        terminal
            .draw(|f| dash.draw_linear_tabs(f, Rect::new(0, 0, 120, 3), &agent))
            .unwrap();
        let fallback = rendered_buffer(&terminal);
        assert!(fallback.contains(long), "{fallback}");
    }
}
