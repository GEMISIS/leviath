//! The stage explorer's state machine, and how the dashboard paints a run
//! onto a stage graph.
//!
//! The canvas itself lives in `crate::tui::flowgraph`; this file is the
//! dashboard's side of it: opening and closing, the keys the explorer owns
//! (the canvas takes the rest), the mouse routing that gives a graph pane
//! first refusal, and [`Dashboard::live_overlay_for`], which turns what the
//! dashboard already reads off disk into the overlay the canvas draws.

use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};

use super::history::{clock, last_visit, visit_count};
use super::state::Dashboard;
use super::types::*;
use crate::tui::flowgraph::{FlowView, LiveOverlay, RunPhase, Selection, StageLive, WorkerCounts};
use leviath_core::run_meta::StageRunStatus;

impl Dashboard {
    /// A detail view opened afresh starts its exploration afresh: the
    /// context tree folded, no explorer, and a band that will start on the
    /// tab that opens.
    pub(super) fn reset_exploration(&mut self) {
        self.context_tree = ContextTreeState::default();
        self.stage_explorer = None;
        self.detail_band = None;
    }

    /// `g` in the detail view: open the explorer on the selected run. Every
    /// run has a graph (a linear blueprint is a chain); the one that has not
    /// is a run whose manifest could not be read, and that is said out loud
    /// rather than shown as an empty canvas.
    pub(super) fn open_stage_explorer(&mut self) {
        let Some(agent) = self.selected_agent() else {
            return;
        };
        let run_id = agent.id.clone();
        match agent.graph.clone() {
            Some(graph) => {
                self.ensure_history(&run_id);
                // The canvas a previous visit left behind for this run comes
                // back as it was: viewport, dragged boxes, direction, toggles.
                let view = match self.explorer_cache.take() {
                    Some((cached_run, view)) if cached_run == run_id => view,
                    _ => {
                        // The explorer is the map: the whole blueprint, with
                        // the run lit on it. The band beside it is already
                        // the path, so opening this on the path too would
                        // show the same picture twice. A canvas the user has
                        // been in before comes back as they left it.
                        let mut view = FlowView::new(graph, false);
                        view.set_show_all(true);
                        view
                    }
                };
                self.stage_explorer = Some(ExplorerState::new(run_id, view));
            }
            None => self.toast(
                "This run's blueprint could not be read, so there is no stage graph to show",
                ToastLevel::Warning,
            ),
        }
    }

    /// Close the explorer, keeping its canvas for the next `g` on the same
    /// run.
    pub(super) fn close_stage_explorer(&mut self) {
        // Nothing open (never the case for the callers here) keeps whatever
        // was cached.
        self.explorer_cache = self
            .stage_explorer
            .take()
            .map(|explorer| (explorer.run_id, explorer.view))
            .or(self.explorer_cache.take());
    }

    /// Advance the edge animation on every open canvas, once per tick.
    pub(super) fn tick_graphs(&mut self, elapsed: std::time::Duration) {
        if let Some(explorer) = self.stage_explorer.as_mut() {
            explorer.view.tick(elapsed);
        }
        if let Some(band) = self.detail_band.as_mut() {
            band.view_mut().tick(elapsed);
        }
        if let Some(Ok(view)) = self.new_run_preview.as_mut().map(|p| p.view.as_mut()) {
            view.tick(elapsed);
        }
        if let Some(screen) = self.agent_builder.as_deref_mut() {
            if let Some((_, Ok(view))) = screen.catalog.preview.as_mut() {
                view.tick(elapsed);
            }
            if let Some(editor) = screen.editor.as_mut() {
                editor.view.tick(elapsed);
            }
        }
    }

    /// Keys while the full-screen stage explorer is open. The explorer owns
    /// closing, the tab switch and help; on the graph tab everything else
    /// goes to the canvas, on the timeline tab to the visit list.
    pub(super) fn handle_explorer_key(&mut self, key_code: KeyCode) {
        let visits = self.selected_history().map(|h| h.visits.len()).unwrap_or(0);
        let Some(explorer) = self.stage_explorer.as_mut() else {
            return;
        };
        match key_code {
            KeyCode::Esc | KeyCode::Char('g') => self.close_stage_explorer(),
            KeyCode::Tab | KeyCode::BackTab => {
                explorer.tab = match explorer.tab {
                    ExplorerTab::Graph => ExplorerTab::Timeline,
                    ExplorerTab::Timeline => ExplorerTab::Graph,
                };
            }
            KeyCode::Char('?') | KeyCode::F(1) => self.show_help = true,
            _ => match explorer.tab {
                ExplorerTab::Graph => match key_code {
                    KeyCode::Enter => {
                        if let Selection::Node(id) = explorer.view.selection() {
                            self.jump_to_stage(&id);
                        }
                    }
                    other => {
                        explorer.view.handle_key(other);
                    }
                },
                ExplorerTab::Timeline => match key_code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        explorer.timeline_selected = explorer.timeline_selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if visits > 0 && explorer.timeline_selected + 1 < visits {
                            explorer.timeline_selected += 1;
                        }
                    }
                    KeyCode::Enter => {
                        let selected = explorer.timeline_selected;
                        let point = self
                            .selected_history()
                            .and_then(|h| h.visits.get(selected))
                            .map(|v| v.first_point);
                        if let Some(point) = point {
                            self.close_stage_explorer();
                            self.jump_to_history_point(point);
                        }
                    }
                    _ => {}
                },
            },
        }
    }

    /// Enter on a selected stage: open its tab in the detail view and close
    /// the explorer. A node that is not a stage of this run (an external
    /// worker blueprint) has no tab, so nothing happens.
    fn jump_to_stage(&mut self, name: &str) {
        let index = self.selected_agent().and_then(|a| {
            a.stages
                .iter()
                .position(|s| s.name == name)
                .or_else(|| a.graph.as_ref().and_then(|g| g.stage_index(name)))
        });
        if let Some(index) = index {
            self.selected_stage = index;
            self.detail_scroll = 0;
            self.review_scroll = 0;
            self.search_mode = false;
            self.search_query.clear();
            self.search_match_idx = 0;
            self.close_stage_explorer();
        }
    }

    /// What the run has done to its blueprint, for the canvas: current stage
    /// and status, per-stage ledger and visits, worker counts, the
    /// transitions actually followed.
    pub(super) fn live_overlay_for(&self, agent: &DashboardAgent) -> LiveOverlay {
        let visits = self
            .history
            .as_ref()
            .filter(|h| h.run_id == agent.id)
            .map(|h| h.visits.as_slice())
            .unwrap_or(&[]);
        let names: Vec<String> = agent
            .graph
            .as_ref()
            .map(|g| g.ids().map(str::to_string).collect())
            .unwrap_or_default();
        let stages = names
            .into_iter()
            .map(|name| {
                let record = agent.stages.iter().find(|s| s.name == name);
                StageLive {
                    // `entered` is the ledger's word; the status is a fallback
                    // for a ledger written before the flag existed. Pending and
                    // Skipped both mean the run never got there.
                    entered: record.is_some_and(|r| {
                        r.entered
                            || !matches!(
                                r.status,
                                StageRunStatus::Pending | StageRunStatus::Skipped
                            )
                    }),
                    errored: record.is_some_and(|r| r.status == StageRunStatus::Error),
                    visits: visit_count(visits, &name),
                    last_seen: last_visit(visits, &name).map(|v| clock(v.entered_at)),
                    // One box per stage here, so a revisited stage has no
                    // single iteration count; the current one's comes
                    // through `LiveOverlay::iteration`.
                    iterations: None,
                    name,
                }
            })
            .collect();
        let taken: Vec<(String, String)> = visits
            .windows(2)
            .map(|pair| (pair[0].stage.clone(), pair[1].stage.clone()))
            .collect();
        let last_transition = taken.last().cloned();

        LiveOverlay {
            current: (!agent.stage.is_empty()).then(|| agent.stage.clone()),
            run: Some(run_phase(&agent.status)),
            iteration: agent.iteration,
            stages,
            workers: self.worker_counts_for(agent),
            taken,
            last_transition,
            tick: self.tick_count,
        }
    }

    /// How the children a fan-out stage spawned are getting on, for the box
    /// of whichever stage the run is in. `None` when the run has no children
    /// at all, which is every run that has not fanned out.
    pub(super) fn worker_counts_for(&self, agent: &DashboardAgent) -> Option<WorkerCounts> {
        let mut workers = WorkerCounts::default();
        let mut has_workers = false;
        for child in self
            .agents
            .iter()
            .filter(|a| a.parent_id.as_deref() == Some(agent.id.as_str()))
        {
            has_workers = true;
            match child.status {
                AgentDisplayStatus::Complete | AgentDisplayStatus::CompleteInteractive => {
                    workers.done += 1;
                }
                AgentDisplayStatus::Error(_) | AgentDisplayStatus::Cancelled => {
                    workers.failed += 1;
                }
                _ => workers.running += 1,
            }
        }
        has_workers.then_some(workers)
    }

    /// Give a graph canvas first refusal on a mouse event. Returns whether the
    /// canvas took it, in which case the text-selection machinery must not
    /// see it (a pan would otherwise start a copy highlight).
    ///
    /// A left press over a canvas captures the mouse until release, so a pan
    /// that leaves the canvas keeps panning and a drag that started elsewhere
    /// stays with text selection. The wheel goes to whatever canvas is under
    /// it. Anything else (the right button, plain motion) is not routed.
    pub(super) fn route_mouse_to_graph(&mut self, event: MouseEvent) -> bool {
        let target = match event.kind {
            MouseEventKind::Down(MouseButton::Left)
            | MouseEventKind::Down(MouseButton::Right)
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown => self
                .mouse_capture
                .or_else(|| self.graph_pane_at(event.column, event.row)),
            MouseEventKind::Drag(MouseButton::Left)
            | MouseEventKind::Up(MouseButton::Left)
            | MouseEventKind::Up(MouseButton::Right) => self.mouse_capture,
            _ => None,
        };
        let Some(id) = target else {
            return false;
        };
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Down(MouseButton::Right) => {
                self.mouse_capture = Some(id);
                self.selection = None;
            }
            MouseEventKind::Up(MouseButton::Left) | MouseEventKind::Up(MouseButton::Right) => {
                self.mouse_capture = None;
            }
            _ => {}
        }
        // The pane may have closed between frames; the event is still ours
        // to swallow, or a stale press would start a text selection.
        if let Some(view) = self.graph_view_mut(id) {
            view.handle_mouse(event);
        }
        true
    }

    /// The graph pane under a terminal cell, from the rects registered this
    /// frame.
    fn graph_pane_at(&self, column: u16, row: u16) -> Option<PaneId> {
        self.pane_rects
            .iter()
            .find(|(id, rect)| {
                id.is_graph()
                    && column >= rect.x
                    && column < rect.x + rect.width
                    && row >= rect.y
                    && row < rect.y + rect.height
            })
            .map(|(id, _)| *id)
    }

    /// The canvas behind a graph pane, if it is showing.
    pub(super) fn graph_view_mut(&mut self, id: PaneId) -> Option<&mut FlowView> {
        match id {
            PaneId::ExplorerGraph => self
                .stage_explorer
                .as_mut()
                .filter(|e| e.tab == ExplorerTab::Graph)
                .map(|e| &mut e.view),
            PaneId::DetailBand => self.detail_band.as_mut().map(|b| b.view_mut()),
            PaneId::NewRunPreview => {
                let open = self.new_run_screen;
                self.new_run_preview
                    .as_mut()
                    .filter(|_| open)
                    .and_then(|p| p.view.as_mut().ok())
            }
            PaneId::AgentsPreview => self
                .agent_builder
                .as_deref_mut()
                .and_then(|s| s.catalog.preview.as_mut())
                .and_then(|(_, view)| view.as_mut().ok()),
            PaneId::AgentEditorGraph => self
                .agent_builder
                .as_deref_mut()
                .and_then(|s| s.editor.as_mut())
                .map(|e| &mut e.view),
            PaneId::RunTable | PaneId::LogPanel => None,
        }
    }
}

/// The canvas's view of a run's status.
pub(super) fn run_phase(status: &AgentDisplayStatus) -> RunPhase {
    match status {
        AgentDisplayStatus::Active => RunPhase::Active,
        AgentDisplayStatus::Waiting => RunPhase::Waiting,
        AgentDisplayStatus::Complete | AgentDisplayStatus::CompleteInteractive => {
            RunPhase::Complete
        }
        AgentDisplayStatus::Error(_) => RunPhase::Error,
        AgentDisplayStatus::Paused => RunPhase::Paused,
        AgentDisplayStatus::Cancelled => RunPhase::Cancelled,
        AgentDisplayStatus::Stale => RunPhase::Stale,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::commands::dashboard::history::{RunHistoryCache, derive_visits};
    use crate::commands::dashboard::test_support::make_test_dashboard;
    use crate::tui::flowgraph::StageGraph;
    use crate::tui::flowgraph::content::NodeStatus;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use leviath_core::manifest::parse_manifest;
    use leviath_core::run_meta::StageRecord;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    fn stage_graph() -> Arc<StageGraph> {
        Arc::new(StageGraph::from_blueprint(
            &parse_manifest(
                r#"
[agent]
name = "grapher"
[stages.plan]
[stages.plan.transitions.implement]
[stages.implement]
mode = "fan_out"
worker_agent = "researcher"
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

    fn agent(id: &str, status: AgentDisplayStatus) -> DashboardAgent {
        DashboardAgent {
            id: id.to_string(),
            blueprint_name: "grapher".to_string(),
            stage: "implement".to_string(),
            stage_index: 1,
            num_stages: 4,
            status,
            tokens_in: 0,
            tokens_out: 0,
            cached_tokens: 0,
            iteration: 3,
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
            started_at: 1000,
            last_progress_at: None,
            runtime_secs: 0,
            clock_now: 0,
            graph: Some(stage_graph()),
            accepts_messages: true,
        }
    }

    fn record(name: &str, status: StageRunStatus) -> StageRecord {
        let mut record = StageRecord::new(name.to_string(), 0);
        record.entered = !matches!(status, StageRunStatus::Pending | StageRunStatus::Skipped);
        record.status = status;
        record
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

    fn seed(dash: &mut Dashboard, run_id: &str, stages: &[(&str, i64)]) {
        let points: Vec<leviath_core::run_archive::RunPoint> = stages
            .iter()
            .map(|(stage, at)| {
                let mut meta = leviath_core::run_meta::RunMeta::new(
                    run_id.to_string(),
                    "a".to_string(),
                    "/p".to_string(),
                    "t".to_string(),
                    None,
                    "/w".to_string(),
                    3,
                );
                meta.current_stage = stage.to_string();
                meta.iteration = 1;
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
            run_id: run_id.to_string(),
            visits: derive_visits(&points),
            points,
            loaded_at_tick: u64::MAX,
        });
    }

    /// A dashboard in the detail view of one graph run, explorer closed.
    fn dash_with_run() -> Dashboard {
        let mut dash = make_test_dashboard();
        dash.agents.push(agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;
        dash
    }

    fn draw(dash: &mut Dashboard) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(200, 50)).unwrap();
        terminal.draw(|f| dash.draw(f)).unwrap();
        terminal
    }

    #[test]
    fn g_opens_the_explorer_for_any_run_with_a_graph_and_toasts_without_one() {
        let mut dash = dash_with_run();
        dash.handle_key(key(KeyCode::Char('g')));
        assert!(dash.stage_explorer.is_some());
        // g again closes it.
        dash.handle_key(key(KeyCode::Char('g')));
        assert!(dash.stage_explorer.is_none());

        // A run whose manifest could not be read has no graph: say so.
        let mut dash = make_test_dashboard();
        let mut unreadable = agent("run-2", AgentDisplayStatus::Active);
        unreadable.graph = None;
        dash.agents.push(unreadable);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.handle_key(key(KeyCode::Char('g')));
        assert!(dash.stage_explorer.is_none());
        assert!(
            dash.toasts
                .iter()
                .any(|t| t.message.contains("could not be read"))
        );

        // No run selected at all: nothing happens, no toast either.
        let mut dash = make_test_dashboard();
        dash.detail_view = true;
        dash.handle_key(key(KeyCode::Char('g')));
        assert!(dash.stage_explorer.is_none());
        assert!(dash.toasts.is_empty());
    }

    #[test]
    fn the_explorer_keys_select_toggle_tab_and_jump() {
        let mut dash = dash_with_run();
        seed(
            &mut dash,
            "run-1",
            &[
                ("plan", 10),
                ("implement", 20),
                ("review", 30),
                ("implement", 40),
            ],
        );
        dash.handle_key(key(KeyCode::Char('g')));
        draw(&mut dash);

        // Graph tab: the canvas takes selection and toggles; help opens.
        dash.handle_key(key(KeyCode::Char(']')));
        assert_eq!(
            dash.stage_explorer.as_ref().unwrap().view.selection(),
            Selection::Node("plan".into())
        );
        // The explorer is the map: it opens on the whole blueprint, and `t`
        // filters it down to the path the run took. (The band beside it is
        // already the path, so opening on that would say the same thing
        // twice.)
        assert!(dash.stage_explorer.as_ref().unwrap().view.show_all());
        dash.handle_key(key(KeyCode::Char('t')));
        assert!(!dash.stage_explorer.as_ref().unwrap().view.show_all());
        dash.handle_key(key(KeyCode::Char('t')));
        assert!(dash.stage_explorer.as_ref().unwrap().view.show_all());
        dash.handle_key(key(KeyCode::Char('e')));
        assert!(dash.stage_explorer.as_ref().unwrap().view.show_escape());
        dash.handle_key(key(KeyCode::Char('r')));
        assert_eq!(
            dash.stage_explorer.as_ref().unwrap().view.direction(),
            crate::tui::flowgraph::Direction::TopToBottom
        );
        let terminal = draw(&mut dash);
        assert!(
            crate::commands::dashboard::test_support::rendered_buffer(&terminal)
                .contains("top to bottom (r)")
        );
        dash.handle_key(key(KeyCode::Char('?')));
        assert!(dash.show_help);
        dash.handle_key(key(KeyCode::Esc)); // closes help
        assert!(dash.stage_explorer.is_some());

        // Enter on the selected stage opens its tab and closes the explorer.
        dash.selected_stage = 1;
        dash.detail_scroll = 7;
        dash.handle_key(key(KeyCode::Enter));
        assert!(dash.stage_explorer.is_none());
        assert_eq!(dash.selected_stage, 0);
        assert_eq!(dash.detail_scroll, 0);

        // Reopening the same run brings its canvas back, selection and
        // toggles included, so Enter opens plan's tab again.
        dash.handle_key(key(KeyCode::Char('g')));
        assert_eq!(
            dash.stage_explorer.as_ref().unwrap().view.selection(),
            Selection::Node("plan".into())
        );
        assert!(dash.stage_explorer.as_ref().unwrap().view.show_all());
        dash.handle_key(key(KeyCode::Enter));
        assert!(dash.stage_explorer.is_none());
        // Enter with nothing selected keeps the explorer open; Enter on an
        // external worker node has no tab to open.
        dash.explorer_cache = None;
        dash.handle_key(key(KeyCode::Char('g')));
        dash.handle_key(key(KeyCode::Enter));
        assert!(dash.stage_explorer.is_some());
        dash.stage_explorer
            .as_mut()
            .unwrap()
            .view
            .select_stage("ext:researcher");
        dash.handle_key(key(KeyCode::Enter));
        assert!(dash.stage_explorer.is_some());
        // A stage the ledger knows resolves through the ledger.
        dash.agents[0].stages = vec![
            record("plan", StageRunStatus::Complete),
            record("implement", StageRunStatus::Active),
        ];
        dash.stage_explorer
            .as_mut()
            .unwrap()
            .view
            .select_stage("implement");
        dash.handle_key(key(KeyCode::Enter));
        assert!(dash.stage_explorer.is_none());
        assert_eq!(dash.selected_stage, 1);

        // Timeline tab: up/down move, Enter jumps to the visit's context.
        dash.handle_key(key(KeyCode::Char('g')));
        dash.handle_key(key(KeyCode::Tab));
        assert_eq!(
            dash.stage_explorer.as_ref().unwrap().tab,
            ExplorerTab::Timeline
        );
        dash.handle_key(key(KeyCode::Down));
        dash.handle_key(key(KeyCode::Char('j')));
        dash.handle_key(key(KeyCode::Down));
        dash.handle_key(key(KeyCode::Down)); // clamped at the last visit
        assert_eq!(dash.stage_explorer.as_ref().unwrap().timeline_selected, 3);
        dash.handle_key(key(KeyCode::Up));
        dash.handle_key(key(KeyCode::Char('k')));
        assert_eq!(dash.stage_explorer.as_ref().unwrap().timeline_selected, 1);
        dash.handle_key(key(KeyCode::Char('z'))); // ignored on the timeline
        dash.handle_key(key(KeyCode::Enter));
        assert!(
            dash.stage_explorer.is_none(),
            "the jump closes the explorer"
        );
        assert_eq!(dash.context_history_idx, Some(1));
        assert_eq!(dash.stage_content_mode, StageContentMode::Context);

        // Shift-Tab cycles back to the graph; Esc closes.
        dash.handle_key(key(KeyCode::Char('g')));
        dash.handle_key(key(KeyCode::Tab));
        dash.handle_key(key(KeyCode::BackTab));
        assert_eq!(
            dash.stage_explorer.as_ref().unwrap().tab,
            ExplorerTab::Graph
        );
        dash.handle_key(key(KeyCode::Esc));
        assert!(dash.stage_explorer.is_none());
    }

    #[test]
    fn explorer_guards_hold_when_driven_directly_or_without_visits() {
        let mut dash = make_test_dashboard();
        // No explorer open: the handler declines to act.
        dash.handle_explorer_key(KeyCode::Enter);
        assert!(dash.stage_explorer.is_none());

        // Timeline Enter with no archived visits is a no-op.
        let mut dash = dash_with_run();
        dash.handle_key(key(KeyCode::Char('g')));
        dash.handle_key(key(KeyCode::Tab));
        dash.handle_key(key(KeyCode::Down));
        dash.handle_key(key(KeyCode::Enter));
        assert!(dash.stage_explorer.is_some(), "nothing to jump to");
        assert_eq!(dash.context_history_idx, None);
        dash.tick_graphs(std::time::Duration::from_millis(100));
    }

    #[test]
    fn the_live_overlay_maps_ledger_visits_workers_and_transitions() {
        let mut dash = dash_with_run();
        seed(
            &mut dash,
            "run-1",
            &[
                ("plan", 10),
                ("implement", 20),
                ("review", 30),
                ("implement", 40),
            ],
        );
        // An older ledger without the `entered` flag still reads a
        // completed stage as entered.
        let mut legacy = record("plan", StageRunStatus::Complete);
        legacy.entered = false;
        dash.agents[0].stages = vec![
            legacy,
            record("implement", StageRunStatus::Active),
            record("review", StageRunStatus::Error),
            record("done", StageRunStatus::Skipped),
        ];
        // Two workers of this run, one finished, one failed; and a stranger.
        let mut done = agent("w-1", AgentDisplayStatus::Complete);
        done.parent_id = Some("run-1".into());
        let mut failed = agent("w-2", AgentDisplayStatus::Cancelled);
        failed.parent_id = Some("run-1".into());
        let mut running = agent("w-3", AgentDisplayStatus::Waiting);
        running.parent_id = Some("run-1".into());
        let stranger = agent("other", AgentDisplayStatus::Active);
        dash.agents.extend([done, failed, running, stranger]);
        dash.tick_count = 42;

        let live = dash.live_overlay_for(&dash.agents[0].clone());
        assert_eq!(live.current.as_deref(), Some("implement"));
        assert_eq!(live.run, Some(RunPhase::Active));
        assert_eq!(live.iteration, 3);
        assert_eq!(live.tick, 42);
        assert_eq!(
            live.workers,
            Some(WorkerCounts {
                running: 1,
                done: 1,
                failed: 1
            })
        );
        assert_eq!(
            live.taken,
            vec![
                ("plan".to_string(), "implement".to_string()),
                ("implement".to_string(), "review".to_string()),
                ("review".to_string(), "implement".to_string()),
            ]
        );
        assert_eq!(
            live.last_transition,
            Some(("review".to_string(), "implement".to_string()))
        );
        let stage = |name: &str| live.stages.iter().find(|s| s.name == name).unwrap().clone();
        assert!(stage("plan").entered && !stage("plan").errored);
        assert_eq!(stage("plan").visits, 1);
        assert!(stage("plan").last_seen.is_some());
        assert_eq!(stage("implement").visits, 2);
        assert!(stage("review").errored);
        assert!(
            !stage("done").entered && stage("done").last_seen.is_none(),
            "a skipped stage was never entered"
        );
        assert!(stage("plan").entered, "status stands in for a missing flag");
        // External nodes are in the overlay too, untouched.
        assert!(!stage("ext:researcher").entered);

        // Applied to the canvas: the fan-out current stage carries the
        // workers, the errored stage shows as such.
        dash.handle_key(key(KeyCode::Char('g')));
        draw(&mut dash);
        let view = &dash.stage_explorer.as_ref().unwrap().view;
        assert_eq!(
            view.node_status("implement"),
            Some(NodeStatus::Current {
                run: RunPhase::Active,
                times: 2
            })
        );
        assert_eq!(
            view.node_status("review"),
            Some(NodeStatus::Visited {
                times: 1,
                errored: true
            })
        );
        assert!(view.edge_animated("review", "implement"));

        // The same run once it finishes: the path it took is still drawn, but
        // nothing on it moves any more. A pulse into the last stage of a run
        // that is over reads as a run still going.
        dash.agents[0].status = AgentDisplayStatus::Complete;
        draw(&mut dash);
        let view = &dash.stage_explorer.as_ref().unwrap().view;
        assert!(
            !view.edge_animated("review", "implement"),
            "a finished run is not travelling anything"
        );
        assert!(!view.edge_hidden("review", "implement"), "still drawn");
        dash.agents[0].status = AgentDisplayStatus::Active;
        draw(&mut dash);
        assert!(
            dash.stage_explorer
                .as_ref()
                .unwrap()
                .view
                .edge_animated("review", "implement"),
            "and it moves again while the run does"
        );

        // A run with no history, no ledger and no stage name: nothing current,
        // no workers, no transitions.
        let mut bare = agent("run-9", AgentDisplayStatus::Paused);
        bare.stage.clear();
        let live = dash.live_overlay_for(&bare);
        assert_eq!(live.current, None);
        assert_eq!(live.run, Some(RunPhase::Paused));
        assert_eq!(live.workers, None);
        assert!(live.taken.is_empty() && live.last_transition.is_none());
        assert!(live.stages.iter().all(|s| !s.entered && s.visits == 0));
        // No graph at all: no stages either.
        bare.graph = None;
        assert!(dash.live_overlay_for(&bare).stages.is_empty());
    }

    #[test]
    fn every_display_status_has_a_run_phase() {
        let cases = [
            (AgentDisplayStatus::Active, RunPhase::Active),
            (AgentDisplayStatus::Waiting, RunPhase::Waiting),
            (AgentDisplayStatus::Complete, RunPhase::Complete),
            (AgentDisplayStatus::CompleteInteractive, RunPhase::Complete),
            (AgentDisplayStatus::Error("boom".into()), RunPhase::Error),
            (AgentDisplayStatus::Paused, RunPhase::Paused),
            (AgentDisplayStatus::Cancelled, RunPhase::Cancelled),
            (AgentDisplayStatus::Stale, RunPhase::Stale),
        ];
        for (status, phase) in cases {
            assert_eq!(run_phase(&status), phase, "{status:?}");
        }
    }

    #[test]
    fn graph_mouse_is_routed_before_text_selection() {
        let mut dash = dash_with_run();
        dash.handle_key(key(KeyCode::Char('g')));
        draw(&mut dash);
        let canvas = dash
            .pane_rects
            .iter()
            .find(|(id, _)| *id == PaneId::ExplorerGraph)
            .map(|(_, r)| *r)
            .expect("the canvas registered itself");
        let inside = (canvas.x + canvas.width / 2, canvas.y + canvas.height - 2);
        let outside = (canvas.x + 1, canvas.y.saturating_sub(1));

        // A press on the canvas captures the mouse and clears any highlight;
        // a drag that leaves the canvas still pans; release lets go.
        let pan = dash.stage_explorer.as_ref().unwrap().view.pan();
        dash.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            inside.0,
            inside.1,
        ));
        assert_eq!(dash.mouse_capture, Some(PaneId::ExplorerGraph));
        assert!(dash.selection.is_none());
        dash.handle_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            outside.0,
            outside.1,
        ));
        assert!(dash.selection.is_none(), "no text selection started");
        dash.handle_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            outside.0,
            outside.1,
        ));
        assert_eq!(dash.mouse_capture, None);
        assert_ne!(dash.stage_explorer.as_ref().unwrap().view.pan(), pan);

        // The wheel over the canvas zooms it rather than scrolling a pane.
        let zoom = dash.stage_explorer.as_ref().unwrap().view.zoom();
        dash.handle_mouse(mouse(MouseEventKind::ScrollDown, inside.0, inside.1));
        assert!(dash.stage_explorer.as_ref().unwrap().view.zoom() < zoom);

        // The right button is the canvas's too (an editor opens a menu on
        // it; the explorer ignores it), held until its release; plain
        // motion is not routed.
        dash.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Right),
            inside.0,
            inside.1,
        ));
        assert_ne!(dash.mouse_capture, None);
        dash.handle_mouse(mouse(
            MouseEventKind::Up(MouseButton::Right),
            inside.0,
            inside.1,
        ));
        // A kind the canvas does not route (the wheel sideways) leaves the
        // capture alone. Plain motion no longer reaches here at all: it is
        // taken above, to light the long-form editor's toolbar.
        dash.handle_mouse(mouse(MouseEventKind::ScrollLeft, inside.0, inside.1));
        assert_eq!(dash.mouse_capture, None);

        // A press outside the canvas starts a text selection as before, and
        // its drag over the canvas stays a text selection.
        dash.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            outside.0,
            outside.1,
        ));
        assert!(dash.selection.is_some());
        assert_eq!(dash.mouse_capture, None);
        dash.handle_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            inside.0,
            inside.1,
        ));
        assert!(dash.selection.is_some());
        dash.handle_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            inside.0,
            inside.1,
        ));

        // A captured press whose canvas closed between frames is swallowed,
        // never handed to text selection.
        dash.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            inside.0,
            inside.1,
        ));
        dash.stage_explorer = None;
        dash.handle_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            inside.0,
            inside.1,
        ));
        assert!(dash.selection.is_none());
        dash.handle_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            inside.0,
            inside.1,
        ));
        assert_eq!(dash.mouse_capture, None);

        // On the timeline tab the canvas is not showing, so the graph pane
        // resolves to no view; text panes never do.
        dash.handle_key(key(KeyCode::Char('g')));
        dash.handle_key(key(KeyCode::Tab));
        assert!(dash.graph_view_mut(PaneId::ExplorerGraph).is_none());
        assert!(dash.graph_view_mut(PaneId::LogPanel).is_none());
        assert!(dash.graph_view_mut(PaneId::RunTable).is_none());
        // Stale rects from the graph tab still route the wheel here, harmlessly.
        dash.pane_rects
            .push((PaneId::ExplorerGraph, Rect::new(0, 0, 10, 10)));
        dash.handle_mouse(mouse(MouseEventKind::ScrollUp, 1, 1));
        assert!(dash.stage_explorer.is_some());
    }
}
