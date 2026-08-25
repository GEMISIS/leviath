//! The detail view's graph band: the blueprint's stage graph in the rows the
//! flat tab strip used to have, drawn by the same canvas as the explorer.
//!
//! The strip said "3 of 7" and which stage was selected; the band says the
//! same and also where the run has been, where it is, and how the stages
//! connect. The selection on the canvas is the stage tab: `←`/`→` move it
//! through the graph, `1`-`9` jump to a stage by number, a click picks a
//! box, and the content panes below follow. Boxes drag, the canvas pans,
//! and `g` opens the full-screen explorer. On a terminal too short to give
//! it its rows the strip stays.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::{Block, BorderType, Borders};

use super::state::Dashboard;
use super::theme::*;
use super::types::*;
use crate::tui::flowgraph::{FlowView, Selection};

/// Rows the band takes: a border, a row of boxes, and lanes for the loops.
pub(super) const BAND_HEIGHT: u16 = 12;

/// The detail area must be at least this tall for the band to replace the
/// three-row strip; below it every other pane needs the rows more.
pub(super) const BAND_MIN_AREA_HEIGHT: u16 = 36;

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

    /// Whether the band drew last frame, so the stage keys go through it.
    pub(super) fn band_shown(&self) -> bool {
        self.pane_rects
            .iter()
            .any(|(id, _)| *id == PaneId::DetailBand)
    }

    /// The tab index of a stage on the band: the ledger's position when it
    /// has one, the blueprint's order otherwise.
    fn band_stage_index(&self, name: &str) -> Option<usize> {
        self.selected_agent().and_then(|a| {
            a.stages
                .iter()
                .position(|s| s.name == name)
                .or_else(|| a.graph.as_ref().and_then(|g| g.stage_index(name)))
        })
    }

    /// The name of the stage tab `index`, the same way round.
    fn band_stage_name(&self, index: usize) -> Option<String> {
        self.selected_agent().and_then(|a| {
            a.stages.get(index).map(|s| s.name.clone()).or_else(|| {
                a.graph
                    .as_ref()
                    .and_then(|g| g.ids().nth(index).map(str::to_string))
            })
        })
    }

    /// The band's selection is the stage tab: adopt it, and reset what a
    /// tab change resets.
    pub(super) fn adopt_band_selection(&mut self) {
        let picked = match self.detail_band.as_ref().map(|b| b.view.selection()) {
            Some(Selection::Node(id)) => self.band_stage_index(&id),
            _ => None,
        };
        if let Some(index) = picked
            && index != self.selected_stage
        {
            self.selected_stage = index;
            self.detail_scroll = 0;
            self.review_scroll = 0;
            self.search_mode = false;
            self.search_query.clear();
            self.search_match_idx = 0;
        }
    }

    /// `←`/`→` (or any canvas key) on the band: move the selection through
    /// the graph and follow it.
    pub(super) fn band_key(&mut self, code: crossterm::event::KeyCode) {
        if let Some(band) = self.detail_band.as_mut() {
            band.view.handle_key(code);
        }
        self.adopt_band_selection();
    }

    /// The tab was set by number: point the band at it.
    pub(super) fn band_select_tab(&mut self, index: usize) {
        if let Some(name) = self.band_stage_name(index)
            && let Some(band) = self.detail_band.as_mut()
        {
            band.view.select_stage(&name);
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
                view: FlowView::new(graph, false),
            });
            // A fresh canvas starts on the tab that is open.
            self.band_select_tab(self.selected_stage);
        } else {
            // A click since the last frame picked a box: that is the tab now.
            self.adopt_band_selection();
        }
        // Visits (the taken edges, the counts) come from the archive.
        self.ensure_history(&agent.id);
        let live = self.live_overlay_for(agent);
        let stage_count = agent
            .stages
            .len()
            .max(agent.graph.as_ref().map(|g| g.stage_count()).unwrap_or(0))
            .max(1);
        let selected = self.selected_stage.min(stage_count - 1);
        let band = self
            .detail_band
            .as_mut()
            .expect("built above when missing or stale");
        band.view.apply_live(&live);
        let title = format!(
            " Stages ←/→ select  1-9 jump  stage {}/{}  ·  [g] graph  ·  click, drag ",
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
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
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
            broken_scripts: Vec::new(),
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

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn record(name: &str, index: usize, entered: bool) -> leviath_core::run_meta::StageRecord {
        let mut r = leviath_core::run_meta::StageRecord::new(name.to_string(), index);
        r.entered = entered;
        r
    }

    /// A run at implement that came through plan: the ledger names both.
    fn run_at_implement(id: &str) -> DashboardAgent {
        let mut run = agent(id);
        run.stages = vec![
            record("plan", 0, true),
            record("implement", 1, true),
            record("review", 2, false),
            record("done", 3, false),
        ];
        run
    }

    #[test]
    fn a_tall_detail_view_draws_the_band_with_the_current_stage_lit_and_the_tab_selected() {
        let mut dash = make_test_dashboard();
        dash.agents.push(run_at_implement("run-1"));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.selected_stage = 0;
        let terminal = draw(&mut dash, 160, 40);
        let text = rendered_buffer(&terminal);
        assert!(text.contains("stage 1/4"), "{text}");
        assert!(text.contains("click, drag"), "{text}");
        // The same picture as the explorer: the path so far and what comes
        // next, in boxes; done hangs off review and waits for its turn.
        for stage in ["plan", "implement", "review"] {
            assert!(text.contains(&format!(" {stage} ")), "{stage}: {text}");
        }
        assert!(!text.contains(" done "), "{text}");
        // The stage the run is in is drawn in the active colour; the selected
        // tab (plan, the open one) has the thick frame; review is dim.
        assert_eq!(style_at_text(&terminal, "implement").fg, Some(C_ACTIVE));
        assert_eq!(style_at_text(&terminal, "┏").fg, Some(C_BORDER_FOCUS));
        assert_eq!(style_at_text(&terminal, "review").fg, Some(C_DIM));
        assert_eq!(
            dash.detail_band.as_ref().unwrap().view.selection(),
            Selection::Node("plan".into())
        );
        assert!(
            dash.pane_rects
                .iter()
                .any(|(id, _)| *id == PaneId::DetailBand)
        );
        assert!(dash.band_shown());
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
        assert!(!text.contains("click, drag"), "{text}");
        assert!(dash.detail_band.is_none());
        assert!(!dash.band_shown());
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
        // Opening the detail view afresh drops the canvas, so the selection
        // starts on the tab that opens.
        dash.open_detail_view();
        assert!(dash.detail_band.is_none());
    }

    #[test]
    fn the_selection_is_the_stage_tab_both_ways() {
        let mut dash = make_test_dashboard();
        dash.agents.push(run_at_implement("run-1"));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.selected_stage = 0;
        draw(&mut dash, 160, 40);
        // `→` walks the graph and the tab follows, resetting the scrolls.
        dash.detail_scroll = 5;
        dash.handle_key(key(KeyCode::Right));
        assert_eq!(dash.selected_stage, 1);
        assert_eq!(dash.detail_scroll, 0);
        assert_eq!(
            dash.detail_band.as_ref().unwrap().view.selection(),
            Selection::Node("implement".into())
        );
        dash.handle_key(key(KeyCode::Right));
        assert_eq!(dash.selected_stage, 2, "review");
        // Nothing to the right of review on show: the tab stays.
        dash.handle_key(key(KeyCode::Right));
        assert_eq!(dash.selected_stage, 2);
        dash.handle_key(key(KeyCode::Left));
        assert_eq!(dash.selected_stage, 1);
        // A number jumps the tab and points the canvas at it.
        dash.handle_key(key(KeyCode::Char('1')));
        assert_eq!(dash.selected_stage, 0);
        assert_eq!(
            dash.detail_band.as_ref().unwrap().view.selection(),
            Selection::Node("plan".into())
        );
        // A selection picked on the canvas (a click) is adopted at the next
        // draw; a selection the ledger does not know is left alone.
        dash.detail_band
            .as_mut()
            .unwrap()
            .view
            .select_stage("review");
        draw(&mut dash, 160, 40);
        assert_eq!(dash.selected_stage, 2);
        dash.detail_band
            .as_mut()
            .unwrap()
            .view
            .select_stage("nowhere");
        dash.adopt_band_selection();
        assert_eq!(dash.selected_stage, 2);
        // Without a band the helpers are inert.
        dash.detail_band = None;
        dash.band_key(KeyCode::Left);
        dash.band_select_tab(0);
        assert_eq!(dash.selected_stage, 2);
        // A run whose ledger is empty falls back to the blueprint's order.
        let mut dash = make_test_dashboard();
        dash.agents.push(agent("run-1"));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.selected_stage = 2;
        draw(&mut dash, 160, 40);
        assert_eq!(
            dash.detail_band.as_ref().unwrap().view.selection(),
            Selection::Node("review".into())
        );
        dash.detail_band
            .as_mut()
            .unwrap()
            .view
            .select_stage("implement");
        dash.adopt_band_selection();
        assert_eq!(dash.selected_stage, 1);
    }

    #[test]
    fn the_band_takes_the_mouse_and_ticks() {
        let mut dash = make_test_dashboard();
        dash.agents.push(run_at_implement("run-1"));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.selected_stage = 1;
        draw(&mut dash, 160, 40);
        // A second draw of the same run keeps the canvas it built.
        let terminal = draw(&mut dash, 160, 40);
        assert_eq!(
            style_at_text(&terminal, "┏").fg,
            Some(C_BORDER_FOCUS),
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
        // A click on a box picks it, and the tab follows at the next draw.
        let (nx, ny, _, _) = dash
            .detail_band
            .as_ref()
            .unwrap()
            .view
            .node_rect("plan")
            .expect("plan is drawn");
        let (nx, ny) = (nx as u16 + 1, ny as u16 + 1);
        dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), nx, ny));
        dash.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), nx, ny));
        draw(&mut dash, 160, 40);
        assert_eq!(dash.selected_stage, 0);
        dash.tick_graphs(std::time::Duration::from_millis(100));
    }
}
