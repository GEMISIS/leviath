//! The detail view's graph band: the blueprint's stage graph, one row per
//! stage box, in the rows the flat tab strip used to have.
//!
//! The strip said "3 of 7" and which stage was selected; the band says the
//! same and also where the run has been, where it is, and how the stages
//! connect. It is a viewer, not a second explorer: `←`/`→` and `1-9` still
//! move the selection, `g` opens the full-screen graph, and the mouse only
//! pans. On a terminal too short to give it its rows the strip stays.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::{Block, BorderType, Borders};

use super::state::Dashboard;
use super::theme::*;
use super::types::*;
use crate::tui::flowgraph::{FlowView, NodeStyle};

/// Rows the band takes: a border, up to three stacked stage boxes with a
/// gap between them, and one lane under them for a revisit loop.
pub(super) const BAND_HEIGHT: u16 = 8;

/// The detail area must be at least this tall for the band to replace the
/// three-row strip; below it every other pane needs the rows more.
pub(super) const BAND_MIN_AREA_HEIGHT: u16 = 32;

/// The band's canvas, kept between frames so the viewport survives a
/// redraw. Rebuilt when the selected run changes.
#[derive(Debug)]
pub(super) struct DetailBand {
    pub(super) run_id: String,
    pub(super) view: FlowView,
}

impl Dashboard {
    /// How tall the stage row of the detail view is: the band when the area
    /// is tall enough and the run has a graph, the flat strip otherwise.
    pub(super) fn stage_row_height(area_height: u16, agent: &DashboardAgent) -> u16 {
        if area_height >= BAND_MIN_AREA_HEIGHT && agent.graph.is_some() {
            BAND_HEIGHT
        } else {
            3
        }
    }

    /// Draw the band into `area`. Returns `false` when the run has no graph
    /// to draw, so the caller can fall back to the strip.
    pub(super) fn draw_stage_band(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        agent: &DashboardAgent,
    ) -> bool {
        let stale = self
            .detail_band
            .as_ref()
            .is_none_or(|b| b.run_id != agent.id);
        if stale {
            let Some(graph) = agent.graph.clone() else {
                return false;
            };
            self.detail_band = Some(DetailBand {
                run_id: agent.id.clone(),
                view: FlowView::new(graph, NodeStyle::Compact, true),
            });
        }
        let live = self.live_overlay_for(agent);
        let stage_count = agent
            .stages
            .len()
            .max(agent.graph.as_ref().map(|g| g.stage_count()).unwrap_or(0))
            .max(1);
        let selected = self.selected_stage.min(stage_count - 1);
        // The ledger names the stage when it has one; the blueprint's own
        // order otherwise (a run that has not written its ledger yet).
        let selected_name = agent
            .stages
            .get(selected)
            .map(|s| s.name.clone())
            .or_else(|| {
                agent
                    .graph
                    .as_ref()
                    .and_then(|g| g.ids().nth(selected).map(str::to_string))
            })
            .unwrap_or_default();

        let band = self
            .detail_band
            .as_mut()
            .expect("built above when missing or stale");
        band.view.apply_live(&live);
        band.view.select_stage(&selected_name);
        let title = format!(
            " Stages ←/→ to switch  stage {}/{}  ·  [g] graph  ·  drag to pan ",
            selected + 1,
            stage_count
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(C_BORDER_FOCUS))
            .title(Span::styled(title, Style::default().fg(C_DIM)));
        let canvas = band.view.render(frame, area, block);
        self.pane_rects.push((PaneId::DetailBand, canvas));
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::commands::dashboard::test_support::{
        make_test_dashboard, rendered_buffer, style_at_text,
    };
    use crate::tui::flowgraph::StageGraph;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
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
[stages.plan.transitions.implement]
[stages.implement]
[stages.implement.transitions.review]
[stages.review]
[stages.review.transitions.implement]
condition = "llm_choice"
[stages.review.transitions.done]
[stages.done]
[stages.done.transitions]
"#,
            )
            .unwrap(),
        ))
    }

    fn agent(id: &str) -> DashboardAgent {
        DashboardAgent {
            id: id.to_string(),
            blueprint_name: "grapher".to_string(),
            stage: "implement".to_string(),
            stage_index: 1,
            num_stages: 4,
            status: AgentDisplayStatus::Active,
            tokens_in: 0,
            tokens_out: 0,
            cached_tokens: 0,
            iteration: 2,
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

    fn draw(dash: &mut Dashboard, w: u16, h: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| dash.draw(f)).unwrap();
        terminal
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn a_tall_detail_view_draws_the_band_with_the_current_stage_lit_and_the_tab_selected() {
        let mut dash = make_test_dashboard();
        dash.agents.push(agent("run-1"));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.selected_stage = 0;
        let terminal = draw(&mut dash, 160, 40);
        let text = rendered_buffer(&terminal);
        assert!(text.contains("stage 1/4"), "{text}");
        assert!(text.contains("drag to pan"), "{text}");
        for stage in ["plan", "implement", "review", "done"] {
            assert!(text.contains(&format!(" {stage} ")), "{stage}: {text}");
        }
        // The stage the run is in is drawn in the active colour; the selected
        // tab is the reversed one; a pending stage is dim.
        assert_eq!(style_at_text(&terminal, "implement").fg, Some(C_ACTIVE));
        assert!(
            style_at_text(&terminal, "plan")
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
        assert_eq!(style_at_text(&terminal, "done").fg, Some(C_DIM));
        assert!(
            dash.pane_rects
                .iter()
                .any(|(id, _)| *id == PaneId::DetailBand)
        );
        // The strip's own chrome is gone.
        assert!(!text.contains("←/→ to switch  stage 1/4 ·  [g]"), "{text}");
    }

    #[test]
    fn a_short_detail_view_keeps_the_flat_strip() {
        let mut dash = make_test_dashboard();
        dash.agents.push(agent("run-1"));
        dash.update_display_indices();
        dash.detail_view = true;
        let terminal = draw(&mut dash, 160, 24);
        let text = rendered_buffer(&terminal);
        assert!(text.contains("stage 1/4"), "{text}");
        assert!(!text.contains("drag to pan"), "{text}");
        assert!(dash.detail_band.is_none());
        assert_eq!(Dashboard::stage_row_height(24, &agent("run-1")), 3);
        assert_eq!(
            Dashboard::stage_row_height(40, &agent("run-1")),
            BAND_HEIGHT
        );
        let mut no_graph = agent("run-2");
        no_graph.graph = None;
        assert_eq!(Dashboard::stage_row_height(40, &no_graph), 3);
    }

    #[test]
    fn the_band_rebuilds_when_the_selected_run_changes_and_declines_without_a_graph() {
        let mut dash = make_test_dashboard();
        dash.agents.push(agent("run-1"));
        let mut other = agent("run-2");
        other.stage = "review".to_string();
        dash.agents.push(other);
        dash.update_display_indices();
        dash.detail_view = true;
        draw(&mut dash, 160, 40);
        assert_eq!(dash.detail_band.as_ref().unwrap().run_id, "run-1");
        dash.selected = 1;
        draw(&mut dash, 160, 40);
        assert_eq!(dash.detail_band.as_ref().unwrap().run_id, "run-2");

        // Called for a run without a graph, the band declines and the caller
        // is told so.
        let mut bare = agent("run-3");
        bare.graph = None;
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        let mut drew = true;
        terminal
            .draw(|f| drew = dash.draw_stage_band(f, f.area(), &bare))
            .unwrap();
        assert!(!drew);
        assert!(rendered_buffer(&terminal).trim().is_empty());
        // The selected stage is clamped to what the run has.
        dash.selected = 0;
        dash.selected_stage = 40;
        let terminal = draw(&mut dash, 160, 40);
        assert!(rendered_buffer(&terminal).contains("stage 4/4"));
    }

    #[test]
    fn the_band_takes_the_mouse_and_ticks() {
        let mut dash = make_test_dashboard();
        let mut run = agent("run-1");
        // A ledger names the stages: the selection follows it rather than
        // the blueprint's order.
        run.stages = vec![
            leviath_core::run_meta::StageRecord::new("plan".to_string(), 0),
            leviath_core::run_meta::StageRecord::new("implement".to_string(), 1),
        ];
        dash.agents.push(run);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.selected_stage = 1;
        draw(&mut dash, 160, 40);
        // A second draw of the same run keeps the canvas it built.
        let terminal = draw(&mut dash, 160, 40);
        assert!(
            style_at_text(&terminal, "implement")
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED),
            "the ledger's second stage is the selected one"
        );
        let canvas = dash
            .pane_rects
            .iter()
            .find(|(id, _)| *id == PaneId::DetailBand)
            .map(|(_, r)| *r)
            .expect("the band registered its canvas");
        let pan = dash.detail_band.as_ref().unwrap().view.pan();
        let (x, y) = (canvas.x + canvas.width - 3, canvas.y + 1);
        dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), x, y));
        assert!(
            dash.selection.is_none(),
            "a press on the band is not a text selection"
        );
        dash.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), x - 10, y));
        dash.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), x - 10, y));
        assert_ne!(dash.detail_band.as_ref().unwrap().view.pan(), pan);
        dash.tick_graphs(std::time::Duration::from_millis(100));
    }
}
