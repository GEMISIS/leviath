//! The detail view's graph band: the run's path through its blueprint, drawn
//! in the rows the flat tab strip used to have by the same canvas as the
//! explorer.
//!
//! The strip said "3 of 7" and which stage was selected. The band says where
//! the run has actually been: one box per stage *visit*, in order, snaking
//! across rows so it stays compact and grows a row at a time while the run is
//! still going (see [`crate::tui::flowgraph::path`]). Three passes through
//! `implement` are three boxes - `implement`, `implement (2)`,
//! `implement (3)` - not one with a `×3` badge, because the order is the
//! story. Painting the run onto the whole blueprint instead answers a
//! different question ("what could it do"), and buries this one in the
//! stages it never went near.
//!
//! The blueprint is still a keypress away: `t` swaps the band to it and back,
//! and `g` opens it full screen. The selection on the canvas is the stage
//! tab: `←`/`→` move it along the path, `1`-`9` jump to a stage by number, a
//! click picks a box, and the content panes below follow. Boxes drag and the
//! canvas pans; `R` re-snakes, throwing away an arrangement made by hand. On
//! a terminal too short to give it its rows the strip stays.

use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::{Block, BorderType, Borders};

use super::history::clock;
use super::state::Dashboard;
use super::theme::*;
use super::types::*;
use crate::tui::flowgraph::path::{self, Visit};
use crate::tui::flowgraph::{
    FlowView, LiveOverlay, Selection, StageGraph, snake_per_row, snake_row_pitch,
};

/// Rows the band takes for a path that fits on one row: a border, a row of
/// boxes, and the gutter a hand-off to the next row runs down.
pub(super) const BAND_HEIGHT: u16 = 12;

/// Rows it will grow to for a path that has wrapped. Past this the canvas
/// pans, and the stage the run is in is kept on screen instead - a band tall
/// enough for every path would leave nothing for the panes below.
pub(super) const BAND_MAX_HEIGHT: u16 = 18;

/// The detail area must be at least this tall for the band to replace the
/// three-row strip; below it every other pane needs the rows more.
pub(super) const BAND_MIN_AREA_HEIGHT: u16 = 36;

/// Which picture the band is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BandMode {
    /// The run's path: where it has been, in order.
    Path,
    /// The whole blueprint: everything it could do.
    Blueprint,
}

/// The band's canvases, kept between frames so the viewport survives a
/// redraw. Rebuilt when the selected run changes.
#[derive(Debug)]
pub(super) struct DetailBand {
    pub(super) run_id: String,
    pub(super) mode: BandMode,
    /// The run's path, snaked.
    path: FlowView,
    /// The whole blueprint, built the first time `t` asks for it - most runs
    /// are watched from start to finish without anyone asking.
    blueprint: Option<FlowView>,
    /// What the path was built from: how many visits, and the stage the run
    /// is in. The canvas is only rebuilt when one of them moves.
    signature: (usize, String),
}

impl DetailBand {
    /// The canvas on show.
    pub(super) fn view(&self) -> &FlowView {
        match (self.mode, &self.blueprint) {
            (BandMode::Blueprint, Some(view)) => view,
            _ => &self.path,
        }
    }

    pub(super) fn view_mut(&mut self) -> &mut FlowView {
        match (self.mode, &mut self.blueprint) {
            (BandMode::Blueprint, Some(view)) => view,
            _ => &mut self.path,
        }
    }
}

/// The band's title: what is on show and how to move around it, in as many
/// words as the width allows.
///
/// Only the keys the band actually answers to are advertised. `r` and `e`
/// belong to the canvas but never reach it here - in the detail view they
/// resume the run and jump to the end of the pane below - so the full screen
/// is where the title points for those.
fn band_title(width: u16, mode: BandMode, visits: usize, stage: usize, stages: usize) -> String {
    let other = match mode {
        BandMode::Path => "blueprint",
        BandMode::Blueprint => "path",
    };
    if width < 100 {
        let what = match mode {
            BandMode::Path => format!("Path · {visits}"),
            BandMode::Blueprint => "Blueprint".to_string(),
        };
        return format!(" {what} · {stage}/{stages} · t:{other} · [g] ");
    }
    let what = match mode {
        BandMode::Path => format!(
            "Path · {visits} visit{} · stage {stage}/{stages}",
            if visits == 1 { "" } else { "s" }
        ),
        BandMode::Blueprint => format!("Blueprint · stage {stage}/{stages}"),
    };
    let arrange = match mode {
        BandMode::Path => "  [R] re-snake",
        BandMode::Blueprint => "",
    };
    format!(" {what}  ·  ←/→ select  1-9 jump  ·  [t] {other}{arrange}  [g] full screen ")
}

impl Dashboard {
    /// How tall the stage row of the detail view is: the band when the area
    /// is tall enough and the run has a graph, the flat strip otherwise.
    ///
    /// A path that has wrapped asks for the rows its later rows need, up to
    /// [`BAND_MAX_HEIGHT`]; a run still on its first row leaves them to the
    /// panes below. Assumes the caller has loaded the run's history - without
    /// it the path is one box and the band opens short, then jumps a frame
    /// later.
    pub(super) fn stage_row_height(&self, area: Rect, agent: &DashboardAgent) -> u16 {
        if area.height < BAND_MIN_AREA_HEIGHT || agent.graph.is_none() {
            return 3;
        }
        let visits = self.path_visits(agent);
        if visits.is_empty() {
            return 3;
        }
        // The widest box the path could ask for, ordinal included, so the
        // rows it needs are counted against the same width the canvas will
        // settle on.
        let longest = visits
            .iter()
            .map(|v| v.stage.chars().count() + " (99)".len())
            .max()
            .unwrap_or(0);
        let rows = visits.len().div_ceil(snake_per_row(longest, area.width));
        u16::try_from(rows.saturating_sub(1))
            .unwrap_or(u16::MAX)
            .saturating_mul(snake_row_pitch())
            .saturating_add(BAND_HEIGHT)
            .min(BAND_MAX_HEIGHT)
    }

    /// Whether the band drew last frame, so the stage keys go through it.
    pub(super) fn band_shown(&self) -> bool {
        self.pane_rects
            .iter()
            .any(|(id, _)| *id == PaneId::DetailBand)
    }

    /// The blueprint stage a box on the band stands for: a path box carries
    /// the visit's ordinal, which is not part of the stage's name.
    fn band_stage_of(&self, id: &str) -> String {
        path::stage_of(id, self.selected_agent().and_then(|a| a.graph.as_deref()))
    }

    /// The tab index of a stage on the band: the ledger's position when it
    /// has one, the blueprint's order otherwise.
    fn band_stage_index(&self, name: &str) -> Option<usize> {
        let name = self.band_stage_of(name);
        self.selected_agent().and_then(|a| {
            a.stages
                .iter()
                .position(|s| s.name == name)
                .or_else(|| a.graph.as_ref().and_then(|g| g.stage_index(&name)))
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
        let picked = match self.detail_band.as_ref().map(|b| b.view().selection()) {
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
            band.view_mut().handle_key(code);
        }
        self.adopt_band_selection();
    }

    /// `t`: swap the band between the run's path and the whole blueprint.
    ///
    /// On the explorer `t` filters the picture on show; here it changes which
    /// picture that is, which is the same question asked of a pane with room
    /// for only one of them.
    pub(super) fn toggle_band_mode(&mut self) {
        let blueprint = self
            .selected_agent()
            .and_then(|a| a.graph.clone())
            .map(|graph| {
                // `t` was asked for the whole blueprint. Opening it filtered
                // down to the path the band was already showing would answer
                // with the same picture in a worse layout.
                let mut view = FlowView::new(graph, false);
                view.set_show_all(true);
                view
            });
        let Some(band) = self.detail_band.as_mut() else {
            return;
        };
        band.mode = match band.mode {
            BandMode::Path => BandMode::Blueprint,
            BandMode::Blueprint => BandMode::Path,
        };
        if band.mode == BandMode::Blueprint && band.blueprint.is_none() {
            band.blueprint = blueprint;
        }
        // The tab the panes below are on is the box the new picture opens on.
        self.band_select_tab(self.selected_stage);
    }

    /// `R`: throw away an arrangement made by hand and lay the band out
    /// again.
    pub(super) fn reset_band_layout(&mut self) {
        if let Some(band) = self.detail_band.as_mut() {
            band.view_mut().reset_layout();
        }
    }

    /// The tab was set by number: point the band at it. On a path that is the
    /// *last* box of that stage - the pass the run is on, or ended on.
    pub(super) fn band_select_tab(&mut self, index: usize) {
        let Some(name) = self.band_stage_name(index) else {
            return;
        };
        let ids: Vec<String> = self
            .detail_band
            .as_ref()
            .map(|b| b.view().graph().ids().map(str::to_string).collect())
            .unwrap_or_default();
        let pick = ids.into_iter().rfind(|id| self.band_stage_of(id) == name);
        if let Some(id) = pick
            && let Some(band) = self.detail_band.as_mut()
        {
            band.view_mut().select_stage(&id);
        }
    }

    /// The run's path as the band draws it: what the archive recorded, plus
    /// the stage the run is in now.
    fn path_visits(&self, agent: &DashboardAgent) -> Vec<Visit> {
        let visits: Vec<Visit> = self
            .history
            .as_ref()
            .filter(|h| h.run_id == agent.id)
            .map(|h| {
                h.visits
                    .iter()
                    .map(|v| Visit {
                        stage: v.stage.clone(),
                        at: Some(clock(v.entered_at)),
                        iterations: v.iterations,
                    })
                    .collect()
            })
            .unwrap_or_default();
        path::path_visits(&visits, &agent.stage)
    }

    /// The path graph and the overlay that paints it.
    fn run_path_for(&self, agent: &DashboardAgent) -> (Arc<StageGraph>, LiveOverlay, usize) {
        let visits = self.path_visits(agent);
        let errored: Vec<String> = agent
            .stages
            .iter()
            .filter(|s| s.status == leviath_core::run_meta::StageRunStatus::Error)
            .map(|s| s.name.clone())
            .collect();
        let graph = path::run_path(agent.graph.as_deref(), &visits);
        let mut live = path::path_overlay(
            &visits,
            &errored,
            super::explorer::run_phase(&agent.status),
            self.tick_count,
        );
        live.workers = self.worker_counts_for(agent);
        (Arc::new(graph), live, visits.len())
    }

    /// Draw the band into `area`. Returns `false` when the run has nothing to
    /// draw, so the caller can fall back to the strip.
    pub(super) fn draw_stage_band(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        agent: &DashboardAgent,
    ) -> bool {
        if agent.graph.is_none() {
            return false;
        }
        // The path (and the visit counts behind it) comes from the archive.
        self.ensure_history(&agent.id);
        let (graph, live, visits) = self.run_path_for(agent);
        if visits == 0 {
            return false;
        }
        let signature = (visits, agent.stage.clone());
        let longest = graph.ids().map(|id| id.chars().count()).max().unwrap_or(0);
        let per_row = snake_per_row(longest, area.width);

        let stale = self
            .detail_band
            .as_ref()
            .is_none_or(|b| b.run_id != agent.id);
        if stale {
            self.detail_band = Some(DetailBand {
                run_id: agent.id.clone(),
                mode: BandMode::Path,
                path: FlowView::new_path(graph, per_row),
                blueprint: None,
                signature,
            });
            // A fresh canvas starts on the tab that is open.
            self.band_select_tab(self.selected_stage);
        } else {
            // A click since the last frame picked a box: that is the tab now.
            self.adopt_band_selection();
            let band = self
                .detail_band
                .as_mut()
                .expect("not stale, so the band is there");
            // The path grows a box at a time, and rebuilding the canvas for a
            // frame that added none would throw the camera away every tick.
            if band.signature != signature {
                band.signature = signature;
                band.path.replace_path(graph);
            }
        }
        let stage_count = agent
            .stages
            .len()
            .max(agent.graph.as_ref().map(|g| g.stage_count()).unwrap_or(0))
            .max(1);
        let selected = self.selected_stage.min(stage_count - 1);
        let blueprint_live = self.live_overlay_for(agent);
        let band = self
            .detail_band
            .as_mut()
            .expect("built above when missing or stale");
        band.path.apply_live(&live);
        if let Some(view) = band.blueprint.as_mut() {
            view.apply_live(&blueprint_live);
        }
        let title = band_title(area.width, band.mode, visits, selected + 1, stage_count);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(C_BORDER_FOCUS))
            .title(Span::styled(title, Style::default().fg(C_DIM)));
        let canvas = band.view_mut().render(frame, area, block);
        self.pane_rects.push((PaneId::DetailBand, canvas));
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::commands::dashboard::history::{RunHistoryCache, derive_visits};
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
            runtime_secs: 0,
            clock_now: 0,
            graph: Some(stage_graph()),
            accepts_messages: true,
            taint_summary: vec![],
        }
    }

    /// Give `run` an archived path: one point per stage named, in order, so
    /// `derive_visits` sees them as consecutive stays.
    fn seed(dash: &mut Dashboard, run_id: &str, stages: &[&str]) {
        let points: Vec<leviath_core::run_archive::RunPoint> = stages
            .iter()
            .enumerate()
            .map(|(i, stage)| {
                let mut meta = leviath_core::run_meta::RunMeta::new(
                    run_id.to_string(),
                    "grapher".to_string(),
                    "/p".to_string(),
                    "t".to_string(),
                    None,
                    "/w".to_string(),
                    4,
                );
                meta.current_stage = (*stage).to_string();
                meta.iteration = 2;
                leviath_core::run_archive::RunPoint {
                    meta,
                    context: leviath_core::run_meta::ContextSnapshot {
                        stage_name: (*stage).to_string(),
                        total_tokens: 0,
                        max_tokens: 100,
                        regions: vec![],
                    },
                    at: 1000 + i as i64 * 10,
                }
            })
            .collect();
        dash.history = Some(RunHistoryCache {
            run_id: run_id.to_string(),
            visits: derive_visits(&points),
            points,
            // Far enough ahead that the TTL never reloads it over a stub
            // loader that would hand back nothing.
            loaded_at_tick: u64::MAX,
        });
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

    /// A run at its second pass through implement, having come through plan
    /// and review: the ledger names the three, the archive has the order.
    fn looped_run(dash: &mut Dashboard, id: &str) {
        let mut run = agent(id);
        run.stages = vec![
            record("plan", 0, true),
            record("implement", 1, true),
            record("review", 2, true),
            record("done", 3, false),
        ];
        dash.agents.push(run);
        dash.update_display_indices();
        dash.detail_view = true;
        seed(dash, id, &["plan", "implement", "review", "implement"]);
    }

    #[test]
    fn the_band_draws_one_box_per_visit_with_the_latest_pass_numbered() {
        let mut dash = make_test_dashboard();
        looped_run(&mut dash, "run-1");
        dash.selected_stage = 0;
        let terminal = draw(&mut dash, 160, 40);
        let text = rendered_buffer(&terminal);

        // Four visits, four boxes: the second pass through implement is its
        // own box, numbered, not a `×2` badge on the first.
        assert!(text.contains("Path · 4 visits"), "{text}");
        assert!(text.contains("implement (2)"), "{text}");
        assert!(!text.contains("implement ×2"), "{text}");
        for stage in ["plan", "implement", "review"] {
            assert!(text.contains(stage), "{stage}: {text}");
        }
        // A stage the run never reached is not on its path at all.
        assert!(!text.contains(" done "), "{text}");
        // The box the run is in is the last one, in the active colour; an
        // earlier pass through the same stage is done.
        assert_eq!(style_at_text(&terminal, "implement (2)").fg, Some(C_ACTIVE));
        assert_eq!(style_at_text(&terminal, "plan").fg, Some(C_WHITE));
        // The selected tab (plan, the open one) has the thick frame.
        assert_eq!(style_at_text(&terminal, "┏").fg, Some(C_BORDER_FOCUS));
        assert_eq!(
            dash.detail_band.as_ref().unwrap().view().selection(),
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
    fn a_stage_the_ledger_calls_errored_is_red_on_the_pass_that_failed() {
        let mut dash = make_test_dashboard();
        looped_run(&mut dash, "run-1");
        let mut failed = record("review", 2, true);
        failed.status = leviath_core::run_meta::StageRunStatus::Error;
        dash.agents[0].stages[2] = failed;
        let terminal = draw(&mut dash, 160, 40);
        assert_eq!(style_at_text(&terminal, "review").fg, Some(C_ERROR));
        // The stages that did not fail are untouched.
        assert_eq!(style_at_text(&terminal, "plan").fg, Some(C_WHITE));
    }

    #[test]
    fn a_run_with_no_archive_yet_still_gets_the_stage_it_is_in() {
        let mut dash = make_test_dashboard();
        dash.agents.push(agent("run-1"));
        dash.update_display_indices();
        dash.detail_view = true;
        seed(&mut dash, "run-1", &[]);
        let terminal = draw(&mut dash, 160, 40);
        let text = rendered_buffer(&terminal);
        assert!(text.contains("Path · 1 visit ·"), "singular: {text}");
        assert!(text.contains("implement"), "{text}");
        assert!(!text.contains("implement (2)"), "{text}");
    }

    #[test]
    fn a_short_detail_view_keeps_the_flat_strip() {
        let mut dash = make_test_dashboard();
        looped_run(&mut dash, "run-1");
        let terminal = draw(&mut dash, 160, 24);
        let text = rendered_buffer(&terminal);
        assert!(text.contains("stage 1/4"), "{text}");
        assert!(!text.contains("Path ·"), "{text}");
        assert!(dash.detail_band.is_none());
        assert!(!dash.band_shown());

        let area = |h: u16| Rect::new(0, 0, 160, h);
        let run = dash.agents[0].clone();
        assert_eq!(dash.stage_row_height(area(24), &run), 3);
        assert_eq!(dash.stage_row_height(area(40), &run), BAND_HEIGHT);
        let mut no_graph = agent("run-2");
        no_graph.graph = None;
        assert_eq!(dash.stage_row_height(area(40), &no_graph), 3);
        // Nothing to draw at all: no archive and no current stage.
        let mut nowhere = agent("run-3");
        nowhere.stage = String::new();
        dash.history = None;
        assert_eq!(dash.stage_row_height(area(40), &nowhere), 3);
    }

    #[test]
    fn the_band_grows_a_row_when_the_path_wraps_and_stops_growing() {
        let mut dash = make_test_dashboard();
        looped_run(&mut dash, "run-1");
        let run = dash.agents[0].clone();
        let area = |w: u16| Rect::new(0, 0, w, 60);
        // Four visits at 160 cells fit one row of four.
        assert_eq!(dash.stage_row_height(area(160), &run), BAND_HEIGHT);
        // The same four on a narrow canvas wrap, and the band grows for it.
        assert_eq!(
            dash.stage_row_height(area(70), &run),
            BAND_HEIGHT + snake_row_pitch()
        );
        // A long path stops at the ceiling and pans instead.
        let many: Vec<&str> = std::iter::repeat_n(["plan", "implement"], 12)
            .flatten()
            .collect();
        seed(&mut dash, "run-1", &many);
        assert_eq!(dash.stage_row_height(area(160), &run), BAND_MAX_HEIGHT);
    }

    #[test]
    fn the_band_rebuilds_when_the_selected_run_changes_and_declines_without_a_graph() {
        let mut dash = make_test_dashboard();
        looped_run(&mut dash, "run-1");
        let mut other = agent("run-2");
        other.stage = "review".to_string();
        dash.agents.push(other);
        dash.update_display_indices();
        draw(&mut dash, 160, 40);
        assert_eq!(dash.detail_band.as_ref().unwrap().run_id, "run-1");
        dash.selected = 1;
        seed(&mut dash, "run-2", &["plan", "review"]);
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
        // A run with a graph but nowhere to be declines too.
        let mut nowhere = agent("run-4");
        nowhere.stage = String::new();
        dash.history = None;
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|f| drew = dash.draw_stage_band(f, f.area(), &nowhere))
            .unwrap();
        assert!(!drew);

        // The selected stage is clamped to what the run has.
        dash.selected = 0;
        dash.selected_stage = 40;
        seed(
            &mut dash,
            "run-1",
            &["plan", "implement", "review", "implement"],
        );
        let terminal = draw(&mut dash, 160, 40);
        assert!(rendered_buffer(&terminal).contains("stage 4/4"));
        // Opening the detail view afresh drops the canvas, so the selection
        // starts on the tab that opens.
        dash.open_detail_view();
        assert!(dash.detail_band.is_none());
    }

    #[test]
    fn a_growing_path_keeps_its_canvas_until_it_actually_grows() {
        let mut dash = make_test_dashboard();
        looped_run(&mut dash, "run-1");
        draw(&mut dash, 160, 40);
        let before = dash.detail_band.as_ref().unwrap().view().graph().clone();
        // A frame that adds no visit redraws the same graph, not a new one.
        draw(&mut dash, 160, 40);
        assert!(Arc::ptr_eq(
            &before,
            dash.detail_band.as_ref().unwrap().view().graph()
        ));
        // One more visit and the path is rebuilt, a box longer.
        seed(
            &mut dash,
            "run-1",
            &["plan", "implement", "review", "implement", "review"],
        );
        let terminal = draw(&mut dash, 160, 40);
        assert!(!Arc::ptr_eq(
            &before,
            dash.detail_band.as_ref().unwrap().view().graph()
        ));
        assert!(rendered_buffer(&terminal).contains("review (2)"));
    }

    #[test]
    fn the_selection_is_the_stage_tab_both_ways() {
        let mut dash = make_test_dashboard();
        looped_run(&mut dash, "run-1");
        dash.selected_stage = 0;
        draw(&mut dash, 160, 40);
        // `→` walks the path and the tab follows, resetting the scrolls.
        dash.detail_scroll = 5;
        dash.handle_key(key(KeyCode::Right));
        assert_eq!(dash.selected_stage, 1, "implement");
        assert_eq!(dash.detail_scroll, 0);
        assert_eq!(
            dash.detail_band.as_ref().unwrap().view().selection(),
            Selection::Node("implement".into())
        );
        dash.handle_key(key(KeyCode::Right));
        assert_eq!(dash.selected_stage, 2, "review");
        // The next box along is the second pass through implement, which is
        // the same tab as the first.
        dash.handle_key(key(KeyCode::Right));
        assert_eq!(dash.selected_stage, 1);
        assert_eq!(
            dash.detail_band.as_ref().unwrap().view().selection(),
            Selection::Node("implement (2)".into())
        );
        dash.handle_key(key(KeyCode::Left));
        assert_eq!(dash.selected_stage, 2);
        // A number jumps the tab and points the canvas at the LAST pass
        // through that stage - where the run is, or ended up.
        dash.handle_key(key(KeyCode::Char('2')));
        assert_eq!(dash.selected_stage, 1);
        assert_eq!(
            dash.detail_band.as_ref().unwrap().view().selection(),
            Selection::Node("implement (2)".into())
        );
        // A selection picked on the canvas (a click) is adopted at the next
        // draw; one the ledger does not know is left alone.
        dash.detail_band
            .as_mut()
            .unwrap()
            .view_mut()
            .select_stage("review");
        draw(&mut dash, 160, 40);
        assert_eq!(dash.selected_stage, 2);
        dash.detail_band
            .as_mut()
            .unwrap()
            .view_mut()
            .select_stage("nowhere");
        dash.adopt_band_selection();
        assert_eq!(dash.selected_stage, 2);
        // Without a band the helpers are inert.
        dash.detail_band = None;
        dash.band_key(KeyCode::Left);
        dash.band_select_tab(0);
        dash.reset_band_layout();
        dash.toggle_band_mode();
        assert_eq!(dash.selected_stage, 2);
    }

    #[test]
    fn a_run_with_no_ledger_falls_back_to_the_blueprints_stage_order() {
        let mut dash = make_test_dashboard();
        dash.agents.push(agent("run-1"));
        dash.update_display_indices();
        dash.detail_view = true;
        seed(&mut dash, "run-1", &["plan", "implement", "review"]);
        dash.selected_stage = 2;
        draw(&mut dash, 160, 40);
        assert_eq!(
            dash.detail_band.as_ref().unwrap().view().selection(),
            Selection::Node("review".into())
        );
        dash.detail_band
            .as_mut()
            .unwrap()
            .view_mut()
            .select_stage("implement");
        dash.adopt_band_selection();
        assert_eq!(dash.selected_stage, 1);
    }

    #[test]
    fn t_swaps_the_band_between_the_path_and_the_whole_blueprint() {
        let mut dash = make_test_dashboard();
        looped_run(&mut dash, "run-1");
        dash.selected_stage = 0;
        let text = rendered_buffer(&draw(&mut dash, 160, 40));
        assert!(text.contains("[t] blueprint"), "{text}");

        dash.handle_key(key(KeyCode::Char('t')));
        let text = rendered_buffer(&draw(&mut dash, 160, 40));
        assert_eq!(dash.detail_band.as_ref().unwrap().mode, BandMode::Blueprint);
        // The whole blueprint: `done` is on it now, and the ordinals are not.
        assert!(text.contains("Blueprint · stage"), "{text}");
        assert!(text.contains("[t] path"), "{text}");
        assert!(text.contains(" done "), "{text}");
        assert!(!text.contains("implement (2)"), "{text}");
        // The run is still painted on it - the revisit count comes back as a
        // badge, which is what a blueprint has room to say.
        assert!(text.contains("implement ×2"), "{text}");
        // Re-snaking a blueprint is not offered.
        assert!(!text.contains("re-snake"), "{text}");

        dash.handle_key(key(KeyCode::Char('t')));
        let text = rendered_buffer(&draw(&mut dash, 160, 40));
        assert_eq!(dash.detail_band.as_ref().unwrap().mode, BandMode::Path);
        assert!(text.contains("implement (2)"), "{text}");
        // With no band on screen `t` is somebody else's key.
        dash.detail_view = false;
        dash.pane_rects.clear();
        dash.handle_key(key(KeyCode::Char('t')));
        assert_eq!(dash.detail_band.as_ref().unwrap().mode, BandMode::Path);
    }

    #[test]
    fn the_band_takes_the_mouse_and_r_puts_a_dragged_box_back() {
        let mut dash = make_test_dashboard();
        looped_run(&mut dash, "run-1");
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
        let pan = dash.detail_band.as_ref().unwrap().view().pan();
        let (x, y) = (canvas.x + canvas.width - 3, canvas.y + 1);
        dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), x, y));
        assert!(
            dash.selection.is_none(),
            "a press on the band is not a text selection"
        );
        dash.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), x - 10, y));
        dash.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), x - 10, y));
        assert_ne!(dash.detail_band.as_ref().unwrap().view().pan(), pan);

        // Drag a box somewhere else and it stays there while the path grows.
        // Asserted in world units, not screen cells: growing the path
        // re-settles the camera, which moves every box on screen whether or
        // not it moved on the canvas.
        let at = |dash: &Dashboard| {
            dash.detail_band
                .as_ref()
                .unwrap()
                .view()
                .positions()
                .get("plan")
                .copied()
                .expect("plan is on the canvas")
        };
        let rect = |dash: &Dashboard| {
            dash.detail_band
                .as_ref()
                .unwrap()
                .view()
                .node_rect("plan")
                .expect("plan is drawn")
        };
        let before = at(&dash);
        let (nx, ny, _, _) = rect(&dash);
        let (nx, ny) = (nx as u16 + 1, ny as u16 + 1);
        dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), nx, ny));
        dash.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), nx, ny + 3));
        dash.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), nx, ny + 3));
        let moved = at(&dash);
        assert_ne!(moved, before, "the drag moved the box");
        seed(
            &mut dash,
            "run-1",
            &["plan", "implement", "review", "implement", "review"],
        );
        draw(&mut dash, 160, 40);
        assert_eq!(at(&dash), moved, "a dragged box survives a new visit");
        // `R` throws the arrangement away and snakes it again.
        dash.handle_key(key(KeyCode::Char('R')));
        draw(&mut dash, 160, 40);
        assert_eq!(at(&dash), before, "re-snaked back to its cell");

        // A click on a box picks it, and the tab follows at the next draw.
        let (nx, ny, _, _) = rect(&dash);
        let (nx, ny) = (nx as u16 + 1, ny as u16 + 1);
        dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), nx, ny));
        dash.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), nx, ny));
        draw(&mut dash, 160, 40);
        assert_eq!(dash.selected_stage, 0);
        dash.tick_graphs(std::time::Duration::from_millis(100));
    }

    #[test]
    fn the_title_says_less_on_a_narrow_band() {
        let wide = band_title(160, BandMode::Path, 4, 2, 4);
        assert!(wide.contains("Path · 4 visits · stage 2/4"), "{wide}");
        assert!(wide.contains("[R] re-snake"), "{wide}");
        let narrow = band_title(80, BandMode::Path, 4, 2, 4);
        assert!(narrow.len() < wide.len(), "{narrow} / {wide}");
        assert!(narrow.contains("Path · 4 · 2/4 · t:blueprint"), "{narrow}");
        let narrow = band_title(80, BandMode::Blueprint, 4, 2, 4);
        assert!(narrow.contains("Blueprint · 2/4 · t:path"), "{narrow}");
        let one = band_title(160, BandMode::Path, 1, 1, 4);
        assert!(one.contains("1 visit ·"), "singular: {one}");
    }
}
