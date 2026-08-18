//! The canvas: a rataflow `Flow` built from a [`StageGraph`], the keys and
//! mouse it answers to, and the per-tick overlay that paints a run onto it.
//!
//! The dashboard owns the event loop and decides which surface an event
//! belongs to; this type only knows what to do with the ones it is handed.
//! It never uses rataflow's default key bindings (whose Delete and Backspace
//! delete nodes): every key maps to an explicit action here.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use rataflow::{
    Edge, FitViewOptions, Flow, FlowAction, Handle, HandlePosition, Node, StepEdge, Theme, Viewport,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Block;

use crate::tui::theme::C_ACCENT;

use super::content::{
    NodeStatus, NodeStyle, RunPhase, StageNodeContent, WorkerCounts, edge_style, palette,
};
use super::layout;
use super::model::{EdgeClass, StageEdge, StageGraph};

/// What a run has done to one stage, for the overlay.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct StageLive {
    pub(crate) name: String,
    /// The run has been in this stage at some point.
    pub(crate) entered: bool,
    /// The stage's ledger says it ended in an error.
    pub(crate) errored: bool,
    /// Distinct visits, from the run archive.
    pub(crate) visits: usize,
    /// When it was last entered, `HH:MM:SS`.
    pub(crate) last_seen: Option<String>,
}

/// Everything the canvas needs to paint a run onto the blueprint, rebuilt
/// by the dashboard each tick from what it already reads off disk.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct LiveOverlay {
    /// The stage the run is in now.
    pub(crate) current: Option<String>,
    pub(crate) run: Option<RunPhase>,
    /// Iterations taken in the current visit.
    pub(crate) iteration: usize,
    pub(crate) stages: Vec<StageLive>,
    /// Workers of the current stage, when it is a fan-out.
    pub(crate) workers: Option<WorkerCounts>,
    /// Transitions the run has followed, as `(from, to)`.
    pub(crate) taken: Vec<(String, String)>,
    /// The most recent transition, animated.
    pub(crate) last_transition: Option<(String, String)>,
    /// The dashboard's tick, for the spinner.
    pub(crate) tick: u64,
}

/// What the user has picked on the canvas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Selection {
    Node(String),
    Edge(StageEdge),
    Nothing,
}

/// One drawn edge, remembered so live restyling can find it.
#[derive(Debug, Clone)]
struct EdgeMeta {
    id: String,
    edge: StageEdge,
    /// Drawn as a loop: its target sits at or before its source on the
    /// canvas (every back-edge does, and so does an edge out of a stage that
    /// is only reachable through escapes).
    loops_back: bool,
    /// How far the edge travels before turning, in world units.
    stem: f64,
}

/// A stage graph on a rataflow canvas.
pub(crate) struct FlowView {
    flow: Flow<StageNodeContent, StepEdge>,
    graph: Arc<StageGraph>,
    edges: Vec<EdgeMeta>,
    show_escape: bool,
    show_unvisited: bool,
    /// A stage to bring on screen at the next draw.
    reveal: Option<String>,
    last_current: Option<String>,
    last_area: Rect,
    canvas: Rect,
    /// The longest lane a visible-by-default edge needs above or below the
    /// nodes, so fitting the view leaves room for it. Escape lanes are not
    /// counted: they are hidden until asked for, and a long one would cost
    /// every fit its width.
    max_stem: f64,
    /// One-row boxes: the canvas cannot zoom out, so it pans instead.
    compact: bool,
}

impl std::fmt::Debug for FlowView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlowView")
            .field("entry", &self.graph.entry)
            .field("nodes", &self.graph.nodes.len())
            .field("show_escape", &self.show_escape)
            .field("show_unvisited", &self.show_unvisited)
            .finish()
    }
}

/// Handles on every side, named by side so an edge can pick where it
/// attaches. Hidden: the canvas is not an editor, and handle glyphs read as
/// ports.
fn handles() -> Vec<Handle> {
    vec![
        Handle::source(HandlePosition::Right)
            .with_id("right")
            .with_hidden(true),
        Handle::source(HandlePosition::Bottom)
            .with_id("bottom")
            .with_offset(0.7)
            .with_hidden(true),
        Handle::source(HandlePosition::Top)
            .with_id("top")
            .with_offset(0.7)
            .with_hidden(true),
        Handle::target(HandlePosition::Left)
            .with_id("left")
            .with_hidden(true),
        Handle::target(HandlePosition::Bottom)
            .with_id("bottom")
            .with_offset(0.3)
            .with_hidden(true),
        Handle::target(HandlePosition::Top)
            .with_id("top")
            .with_offset(0.3)
            .with_hidden(true),
    ]
}

impl FlowView {
    /// Build the canvas for `graph`. `locked` makes it a viewer: left-drag
    /// pans and nothing selects, for the band and the preview.
    pub(crate) fn new(graph: Arc<StageGraph>, style: NodeStyle, locked: bool) -> Self {
        let layout = layout::layout(&graph);
        let longest = graph
            .nodes
            .iter()
            .map(|n| n.id.trim_start_matches("ext:").chars().count())
            .max()
            .unwrap_or(0);
        let node_w = style.width(longest);
        let node_h = style.height();
        // A one-row box has no room to shrink: below zoom 1 it is zero rows
        // tall and gone. Compact canvases stay at 1.0 and pan instead.
        let (gap_x, gap_y, min_zoom) = match style {
            NodeStyle::Full => (8.0, 1.0, 0.4),
            NodeStyle::Compact => (6.0, 1.0, 1.0),
        };

        let nodes: Vec<Node<StageNodeContent>> = graph
            .nodes
            .iter()
            .map(|n| {
                Node::new(
                    n.id.clone(),
                    (0.0, 0.0),
                    (node_w, node_h),
                    StageNodeContent::from_node(n, style),
                )
                .with_handles(handles())
                .with_connectable(false)
                .with_deletable(false)
                .with_resizable(false)
                .with_draggable(false)
            })
            .collect();

        let mut metas: Vec<EdgeMeta> = Vec::with_capacity(graph.edges.len());
        let mut max_stem: f64 = 1.0;
        let edges: Vec<Edge<StepEdge>> = graph
            .edges
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let from_layer = layout.layer_of(&e.from).unwrap_or(0);
                let to_layer = layout.layer_of(&e.to).unwrap_or(0);
                let loops_back = to_layer <= from_layer;
                let (src, tgt) = match e.class {
                    EdgeClass::Escape => (HandlePosition::Top, HandlePosition::Top),
                    _ if loops_back => (HandlePosition::Bottom, HandlePosition::Bottom),
                    _ => (HandlePosition::Right, HandlePosition::Left),
                };
                // Edges that leave and re-enter on the same side run along a
                // lane above or below the nodes; a lane per layer of distance
                // keeps a long loop from overwriting a short one's label.
                let stem = if src == tgt {
                    1.0 + from_layer.abs_diff(to_layer) as f64
                } else {
                    1.0
                };
                if e.class != EdgeClass::Escape {
                    max_stem = max_stem.max(stem);
                }
                let id = format!("e{i}");
                metas.push(EdgeMeta {
                    id: id.clone(),
                    edge: e.clone(),
                    loops_back,
                    stem,
                });
                let mut edge = Edge::new(id, e.from.clone(), e.to.clone())
                    .with_content(styled_edge(e.class, loops_back, false, stem))
                    .with_source_side(src)
                    .with_target_side(tgt)
                    .with_deletable(false)
                    .with_hidden(e.class == EdgeClass::Escape);
                let label = e.condition_label();
                if !label.is_empty() {
                    edge = edge.with_label(format!("[{label}]"));
                }
                edge
            })
            .collect();

        let mut flow = Flow::with_graph(nodes, edges)
            .expect("a StageGraph has unique ids, no self-loops and no dangling edges")
            .with_theme(Theme::Custom(palette()))
            .with_min_zoom(min_zoom)
            .with_max_zoom(1.0)
            // A press on empty canvas is how a pan starts; it must not throw
            // the selection away, or Enter after a pan opens nothing.
            .with_deselect_on_pane_click(false)
            .with_locked(locked);
        flow.set_node_positions(layout.positions(node_w, node_h, gap_x, gap_y));
        // No fit here: the first `render` settles the viewport for the area it
        // gets (a fit, or the top-left corner for a compact canvas that
        // overflows). A fit requested now would land after that and undo it.

        Self {
            flow,
            graph,
            edges: metas,
            show_escape: false,
            show_unvisited: true,
            reveal: None,
            last_current: None,
            last_area: Rect::default(),
            canvas: Rect::default(),
            max_stem,
            compact: style == NodeStyle::Compact,
        }
    }

    /// The longest edge lane, in rows above or below the nodes.
    pub(crate) fn max_stem(&self) -> f64 {
        self.max_stem
    }

    /// The graph this canvas draws.
    pub(crate) fn graph(&self) -> &Arc<StageGraph> {
        &self.graph
    }

    /// The canvas rect from the last draw (empty before the first).
    #[cfg(test)]
    pub(crate) fn canvas(&self) -> Rect {
        self.canvas
    }

    /// Escape edges shown or hidden.
    pub(crate) fn show_escape(&self) -> bool {
        self.show_escape
    }

    /// Unvisited stages shown or hidden.
    pub(crate) fn show_unvisited(&self) -> bool {
        self.show_unvisited
    }

    /// The zoom level, for hint text and tests.
    pub(crate) fn zoom(&self) -> f64 {
        self.flow.viewport.zoom
    }

    /// Where the viewport is panned to.
    #[cfg(test)]
    pub(crate) fn pan(&self) -> (f64, f64) {
        (self.flow.viewport.x, self.flow.viewport.y)
    }

    /// The canvas cell rectangle of a node, once drawn.
    #[cfg(test)]
    pub(crate) fn node_rect(&self, id: &str) -> Option<(i32, i32, i32, i32)> {
        self.flow.node_terminal_rect(id)
    }

    /// Whether a node is drawn.
    #[cfg(test)]
    pub(crate) fn node_hidden(&self, id: &str) -> bool {
        self.flow.node(id).is_some_and(|n| n.hidden)
    }

    /// Whether the edge `from -> to` is drawn.
    #[cfg(test)]
    pub(crate) fn edge_hidden(&self, from: &str, to: &str) -> bool {
        self.edges
            .iter()
            .filter(|m| m.edge.from == from && m.edge.to == to)
            .all(|m| self.flow.edge(&m.id).is_some_and(|e| e.hidden))
    }

    /// Whether the edge `from -> to` is animated.
    #[cfg(test)]
    pub(crate) fn edge_animated(&self, from: &str, to: &str) -> bool {
        self.edges
            .iter()
            .filter(|m| m.edge.from == from && m.edge.to == to)
            .any(|m| self.flow.edge(&m.id).is_some_and(|e| e.animated))
    }

    /// The live status of a node, as last applied.
    #[cfg(test)]
    pub(crate) fn node_status(&self, id: &str) -> Option<NodeStatus> {
        self.flow.node(id).map(|n| n.content.status)
    }

    /// What is selected.
    pub(crate) fn selection(&self) -> Selection {
        if let Some(id) = self.flow.first_selected_node_id() {
            return Selection::Node(id);
        }
        if let Some(id) = self.flow.first_selected_edge_id()
            && let Some(meta) = self.edges.iter().find(|m| m.id == id)
        {
            return Selection::Edge(meta.edge.clone());
        }
        Selection::Nothing
    }

    /// Select a stage by name: the detail band follows the stage tabs this
    /// way, and it works on a locked canvas.
    pub(crate) fn select_stage(&mut self, id: &str) {
        self.flow.select_node(id);
    }

    /// First look at a canvas of this size: fit the graph when it fits, and
    /// otherwise, on a canvas that cannot zoom out (compact boxes), start at
    /// the top-left corner so the entry stage is on screen rather than the
    /// middle of the picture. A full canvas zooms out to fit instead.
    fn settle(&mut self, area: Rect) {
        // Inside the block's border.
        let (inner_w, inner_h) = (f64::from(area.width) - 2.0, f64::from(area.height) - 2.0);
        let (world_w, world_h) = self.world_extent();
        let overflows = world_w + 2.0 > inner_w || world_h + 2.0 > inner_h;
        if self.compact && overflows {
            self.flow.viewport = Viewport {
                x: 1.0,
                y: 1.0,
                zoom: 1.0,
            };
        } else {
            self.fit();
        }
    }

    /// Bring the whole graph on screen at the next draw.
    pub(crate) fn fit(&mut self) {
        self.flow
            .request_fit_view_with_options(fit_options(self.max_stem));
    }

    /// Advance edge animation.
    pub(crate) fn tick(&mut self, elapsed: Duration) {
        self.flow.tick_animation(elapsed);
    }

    /// Show or hide the escape edges.
    pub(crate) fn toggle_escape(&mut self) {
        self.show_escape = !self.show_escape;
        self.sync_visibility();
    }

    /// Show or hide stages the run has never entered.
    pub(crate) fn toggle_unvisited(&mut self) {
        self.show_unvisited = !self.show_unvisited;
        self.sync_visibility();
    }

    /// Keys the canvas answers to. Returns whether it took the key.
    pub(crate) fn handle_key(&mut self, code: KeyCode) -> bool {
        let action = match code {
            KeyCode::Up | KeyCode::Char('k') => Some(FlowAction::SelectUp),
            KeyCode::Down | KeyCode::Char('j') => Some(FlowAction::SelectDown),
            KeyCode::Left | KeyCode::Char('h') => Some(FlowAction::SelectLeft),
            KeyCode::Right | KeyCode::Char('l') => Some(FlowAction::SelectRight),
            KeyCode::Char('[') => Some(FlowAction::SelectPrev),
            KeyCode::Char(']') => Some(FlowAction::SelectNext),
            _ => None,
        };
        if let Some(action) = action {
            let _ = self.flow.apply(action);
            return true;
        }
        match code {
            KeyCode::Char('+') | KeyCode::Char('=') => self.flow.zoom_in(),
            KeyCode::Char('-') => self.flow.zoom_out(),
            KeyCode::Char('0') => self.flow.reset_zoom(),
            KeyCode::Char('f') => self.fit(),
            KeyCode::Char('e') => self.toggle_escape(),
            KeyCode::Char('u') => self.toggle_unvisited(),
            _ => return false,
        }
        true
    }

    /// Mouse on the canvas: left button pans or selects, the wheel zooms at
    /// the cursor. Anything else is left alone. Returns whether the event
    /// was forwarded.
    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) -> bool {
        let forward = matches!(
            event.kind,
            MouseEventKind::Down(MouseButton::Left)
                | MouseEventKind::Drag(MouseButton::Left)
                | MouseEventKind::Up(MouseButton::Left)
                | MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
        );
        if forward {
            let _ = self.flow.handle_mouse_event(event);
        }
        forward
    }

    /// Paint the run onto the blueprint. Cheap enough to call every draw:
    /// it mutates node content in place and never rebuilds the canvas.
    pub(crate) fn apply_live(&mut self, live: &LiveOverlay) {
        let current = live.current.as_deref();
        for (id, content) in self.flow.nodes_content_mut() {
            content.clear_live();
            content.tick = live.tick;
            let record = live.stages.iter().find(|s| s.name == id);
            let visits = record.map(|r| r.visits).unwrap_or(0);
            content.last_seen = record.and_then(|r| r.last_seen.clone());
            if Some(id) == current {
                content.status = NodeStatus::Current {
                    run: live.run.unwrap_or(RunPhase::Active),
                    times: visits.max(1),
                };
                content.iteration = Some(live.iteration);
                if content.kind_label == "fan-out" {
                    content.workers = live.workers;
                }
            } else if let Some(r) = record
                && (r.entered || r.visits > 0)
            {
                content.status = NodeStatus::Visited {
                    times: visits.max(1),
                    errored: r.errored,
                };
            }
        }

        let taken: HashSet<(&str, &str)> = live
            .taken
            .iter()
            .map(|(f, t)| (f.as_str(), t.as_str()))
            .collect();
        let last = live
            .last_transition
            .as_ref()
            .map(|(f, t)| (f.as_str(), t.as_str()));
        for meta in &self.edges {
            let key = (meta.edge.from.as_str(), meta.edge.to.as_str());
            let content = self
                .flow
                .edge_content_mut(&meta.id)
                .expect("every remembered edge is on the canvas");
            *content = styled_edge(
                meta.edge.class,
                meta.loops_back,
                taken.contains(&key),
                meta.stem,
            );
            self.flow.set_edge_animated(&meta.id, last == Some(key));
        }

        if live.current != self.last_current {
            self.reveal = live.current.clone();
            self.last_current = live.current.clone();
        }
        self.sync_visibility();
    }

    /// Hidden nodes and edges follow the toggles: a node hides when it is
    /// unvisited and unvisited stages are off; an edge hides when it is an
    /// escape with escapes off, or when either end is hidden (the canvas
    /// would otherwise draw an arrow into nothing).
    fn sync_visibility(&mut self) {
        let hidden_nodes: Vec<String> = self
            .flow
            .nodes()
            .filter(|n| !self.show_unvisited && n.content.status == NodeStatus::Pending)
            .map(|n| n.id.clone())
            .collect();
        let hidden: HashSet<&str> = hidden_nodes.iter().map(String::as_str).collect();
        for id in self.graph.ids() {
            self.flow.set_node_hidden(id, hidden.contains(id));
        }
        for meta in &self.edges {
            let hide = (meta.edge.class == EdgeClass::Escape && !self.show_escape)
                || hidden.contains(meta.edge.from.as_str())
                || hidden.contains(meta.edge.to.as_str());
            self.flow.set_edge_hidden(&meta.id, hide);
        }
    }

    /// Draw the canvas into `area`, inside `block`. Returns the canvas rect
    /// (the block's inside), which is what mouse routing hit-tests.
    pub(crate) fn render(&mut self, frame: &mut Frame, area: Rect, block: Block<'static>) -> Rect {
        self.flow.set_block(Some(block));
        if (area.width, area.height) != (self.last_area.width, self.last_area.height) {
            self.last_area = area;
            self.settle(area);
        }
        if let Some(id) = self.reveal.take() {
            self.flow.ensure_node_visible(&id);
        }
        frame.render_widget(&mut self.flow, area);
        self.canvas = self.flow.canvas_area();
        self.canvas
    }

    /// Select the `index`-th edge of the graph, for tests that need an edge
    /// picked without a mouse.
    #[cfg(test)]
    pub(crate) fn select_edge_for_test(&mut self, index: usize) {
        self.flow.clear_selection();
        self.flow.select_edge(&format!("e{index}"));
    }

    /// The far corner of the laid-out graph in world units.
    pub(crate) fn world_extent(&self) -> (f64, f64) {
        self.flow
            .nodes()
            .map(|n| n.bounds())
            .fold((0.0_f64, 0.0_f64), |(w, h), b| {
                (
                    w.max(b.position.x + b.dimensions.width),
                    h.max(b.position.y + b.dimensions.height),
                )
            })
    }

    /// The canvas itself, mutably, for the text render.
    pub(super) fn flow_mut(&mut self) -> &mut Flow<StageNodeContent, StepEdge> {
        &mut self.flow
    }
}

fn styled_edge(class: EdgeClass, loops_back: bool, taken: bool, stem: f64) -> StepEdge {
    StepEdge::default()
        .with_stem_length(stem)
        .with_style(edge_style(class, loops_back, taken))
        .with_selected_style(
            edge_style(class, loops_back, taken).with_stroke_style(Style::default().fg(C_ACCENT)),
        )
}

/// Fit with room for the edge lanes above and below the nodes. Capped: a
/// very long loop is better clipped than every node shrunk to make room.
fn fit_options(max_stem: f64) -> FitViewOptions {
    FitViewOptions::default()
        .with_padding((max_stem + 1.0).min(5.0))
        .with_max_zoom(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use leviath_core::manifest::parse_manifest;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn graph() -> Arc<StageGraph> {
        Arc::new(StageGraph::from_blueprint(
            &parse_manifest(
                r#"
[agent]
name = "g"
[stages.plan]
[stages.plan.transitions.implement]
[stages.implement]
[stages.implement.transitions.review]
[stages.implement.transitions.recover]
condition = "error"
[stages.review]
[stages.review.transitions.implement]
condition = "llm_choice"
[stages.review.transitions.done]
[stages.recover]
[stages.recover.transitions.plan]
[stages.done]
mode = "output"
[stages.done.transitions]
"#,
            )
            .unwrap(),
        ))
    }

    fn view() -> FlowView {
        FlowView::new(graph(), NodeStyle::Full, false)
    }

    fn draw(view: &mut FlowView, w: u16, h: u16) -> (Terminal<TestBackend>, String) {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| {
                view.render(f, f.area(), Block::bordered());
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        (terminal, text)
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
    fn new_lays_out_hides_escape_edges_and_draws_every_stage() {
        let mut v = view();
        assert!(
            v.edge_hidden("implement", "recover"),
            "escape hidden by default"
        );
        assert!(!v.edge_hidden("plan", "implement"));
        assert!(!v.node_hidden("recover"));
        assert_eq!(v.canvas(), Rect::default(), "no canvas before a draw");
        let (_, text) = draw(&mut v, 220, 50);
        for stage in ["plan", "implement", "review", "recover", "done"] {
            assert!(text.contains(stage), "{stage} in {text}");
        }
        assert!(text.contains("[llm_choice]"), "condition label: {text}");
        assert!(!text.contains("[error]"), "hidden escape label: {text}");
        assert_ne!(v.canvas(), Rect::default());
        assert_eq!(v.graph().entry, "plan");
        assert!(format!("{v:?}").contains("nodes: 5"));
        // The output stage is not tagged as unvisited (its status is Pending
        // like every other, before a run).
        assert_eq!(v.node_status("done"), Some(NodeStatus::Pending));
        assert_eq!(v.node_status("ghost"), None);
    }

    #[test]
    fn keys_select_zoom_reset_fit_and_toggle() {
        let mut v = view();
        draw(&mut v, 220, 50);
        assert_eq!(v.selection(), Selection::Nothing);
        assert!(v.handle_key(KeyCode::Char(']')));
        assert_eq!(v.selection(), Selection::Node("plan".into()));
        assert!(v.handle_key(KeyCode::Right));
        assert_eq!(v.selection(), Selection::Node("implement".into()));
        assert!(v.handle_key(KeyCode::Char('l')));
        assert!(v.handle_key(KeyCode::Char('h')));
        assert_eq!(v.selection(), Selection::Node("implement".into()));
        assert!(v.handle_key(KeyCode::Char('[')));
        assert_eq!(v.selection(), Selection::Node("plan".into()));
        // Up/down and their aliases are accepted even with nowhere to go.
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Char('k'),
            KeyCode::Char('j'),
            KeyCode::Left,
        ] {
            assert!(v.handle_key(code));
        }
        let before = v.zoom();
        assert!(v.handle_key(KeyCode::Char('-')));
        assert!(v.zoom() < before);
        assert!(v.handle_key(KeyCode::Char('+')));
        assert!(v.handle_key(KeyCode::Char('=')));
        assert!(v.zoom() <= 1.0, "zoom is capped at 1.0");
        assert!(v.handle_key(KeyCode::Char('-')));
        assert!(v.handle_key(KeyCode::Char('0')));
        let reset = v.zoom();
        assert!((reset - 1.0).abs() < 1e-9, "{reset}");
        assert!(v.handle_key(KeyCode::Char('e')));
        assert!(v.show_escape());
        assert!(!v.edge_hidden("implement", "recover"));
        assert!(v.handle_key(KeyCode::Char('u')));
        assert!(!v.show_unvisited());
        // Nothing has run, so every node is unvisited and hides, edges too.
        assert!(v.node_hidden("plan"));
        assert!(v.edge_hidden("plan", "implement"));
        assert!(v.handle_key(KeyCode::Char('u')));
        assert!(!v.node_hidden("plan"));
        assert!(v.handle_key(KeyCode::Char('f')));
        assert!(!v.handle_key(KeyCode::Char('x')));
        assert!(!v.handle_key(KeyCode::Enter));
        let (_, text) = draw(&mut v, 220, 50);
        assert!(text.contains("[error]"), "escape edges revealed: {text}");
    }

    #[test]
    fn apply_live_maps_statuses_reveals_a_changed_stage_and_styles_edges() {
        let mut v = view();
        draw(&mut v, 220, 50);
        let live = LiveOverlay {
            current: Some("review".into()),
            run: Some(RunPhase::Active),
            iteration: 2,
            stages: vec![
                StageLive {
                    name: "plan".into(),
                    entered: true,
                    errored: false,
                    visits: 1,
                    last_seen: Some("10:00:00".into()),
                },
                StageLive {
                    name: "implement".into(),
                    entered: true,
                    errored: true,
                    visits: 2,
                    last_seen: None,
                },
                StageLive {
                    name: "review".into(),
                    entered: true,
                    errored: false,
                    visits: 0,
                    last_seen: None,
                },
                StageLive {
                    name: "done".into(),
                    entered: false,
                    errored: false,
                    visits: 0,
                    last_seen: None,
                },
            ],
            workers: Some(WorkerCounts {
                running: 1,
                done: 0,
                failed: 0,
            }),
            taken: vec![
                ("plan".into(), "implement".into()),
                ("implement".into(), "review".into()),
            ],
            last_transition: Some(("implement".into(), "review".into())),
            tick: 4,
        };
        v.apply_live(&live);
        assert_eq!(
            v.node_status("review"),
            Some(NodeStatus::Current {
                run: RunPhase::Active,
                times: 1
            })
        );
        assert_eq!(
            v.node_status("plan"),
            Some(NodeStatus::Visited {
                times: 1,
                errored: false
            })
        );
        assert_eq!(
            v.node_status("implement"),
            Some(NodeStatus::Visited {
                times: 2,
                errored: true
            })
        );
        assert_eq!(v.node_status("done"), Some(NodeStatus::Pending));
        assert_eq!(v.node_status("recover"), Some(NodeStatus::Pending));
        assert!(v.edge_animated("implement", "review"));
        assert!(!v.edge_animated("plan", "implement"));
        // A non-fan-out current stage never shows worker counts.
        let (_, text) = draw(&mut v, 220, 50);
        assert!(!text.contains(" run ·"), "{text}");
        assert!(text.contains("iter 2"), "{text}");
        assert!(text.contains("10:00:00"), "{text}");
        assert!(text.contains("implement ×2"), "{text}");

        // With unvisited hidden, the pending stages and their edges go.
        v.toggle_unvisited();
        assert!(v.node_hidden("done") && v.node_hidden("recover"));
        assert!(v.edge_hidden("review", "done"));
        assert!(!v.node_hidden("review"));

        // Moving on: the new current stage is revealed at the next draw, the
        // last one becomes visited, and a run that finished spins nothing.
        let mut next = live.clone();
        next.current = Some("done".into());
        next.run = Some(RunPhase::Complete);
        next.stages[3].entered = true;
        v.apply_live(&next);
        assert_eq!(v.reveal.as_deref(), Some("done"));
        draw(&mut v, 220, 50);
        assert!(v.reveal.is_none(), "consumed by the draw");
        assert_eq!(
            v.node_status("done"),
            Some(NodeStatus::Current {
                run: RunPhase::Complete,
                times: 1
            })
        );
        assert_eq!(
            v.node_status("review"),
            Some(NodeStatus::Visited {
                times: 1,
                errored: false
            })
        );
        // Same current again: no new reveal.
        v.apply_live(&next);
        assert!(v.reveal.is_none());
    }

    #[test]
    fn apply_live_ignores_a_current_stage_missing_from_the_blueprint_and_no_run_phase() {
        let mut v = view();
        v.apply_live(&LiveOverlay {
            current: Some("ghost".into()),
            run: None,
            ..LiveOverlay::default()
        });
        assert!(
            v.graph()
                .ids()
                .all(|id| v.node_status(id) == Some(NodeStatus::Pending))
        );
        // A current stage with no phase reported reads as running.
        v.apply_live(&LiveOverlay {
            current: Some("plan".into()),
            run: None,
            ..LiveOverlay::default()
        });
        assert_eq!(
            v.node_status("plan"),
            Some(NodeStatus::Current {
                run: RunPhase::Active,
                times: 1
            })
        );
    }

    #[test]
    fn a_fan_out_current_stage_shows_its_workers() {
        let g = Arc::new(StageGraph::from_blueprint(
            &parse_manifest(
                r#"
[agent]
name = "fan"
[stages.split]
mode = "fan_out"
worker_agent = "researcher"
merge_stage = "merge"
[stages.split.transitions.merge]
[stages.merge]
"#,
            )
            .unwrap(),
        ));
        let mut v = FlowView::new(g, NodeStyle::Full, false);
        v.apply_live(&LiveOverlay {
            current: Some("split".into()),
            run: Some(RunPhase::Waiting),
            workers: Some(WorkerCounts {
                running: 3,
                done: 1,
                failed: 0,
            }),
            ..LiveOverlay::default()
        });
        let (_, text) = draw(&mut v, 160, 40);
        assert!(text.contains("3 run · 1 done · 0 fail"), "{text}");
        assert!(text.contains("researcher"), "external worker node: {text}");
    }

    #[test]
    fn mouse_click_selects_the_node_under_the_cursor_and_ignores_the_right_button() {
        let mut v = view();
        draw(&mut v, 220, 50);
        let (x, y, _, _) = v.node_rect("plan").expect("drawn");
        let (x, y) = (x as u16 + 1, y as u16 + 1);
        assert!(v.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), x, y)));
        assert!(v.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), x, y)));
        assert_eq!(v.selection(), Selection::Node("plan".into()));
        assert!(!v.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Right), x, y)));
        assert!(!v.handle_mouse(mouse(MouseEventKind::Moved, x, y)));
        // Selecting an edge reports the transition it stands for.
        v.flow.clear_selection();
        v.flow.select_edge("e0");
        assert_eq!(v.selection(), Selection::Edge(v.graph().edges[0].clone()));
        v.select_stage("review");
        assert_eq!(v.selection(), Selection::Node("review".into()));
    }

    #[test]
    fn mouse_drag_pans_and_wheel_zooms_also_when_locked() {
        for locked in [false, true] {
            let mut v = FlowView::new(graph(), NodeStyle::Compact, locked);
            draw(&mut v, 60, 12);
            v.select_stage("plan");
            let pan = v.pan();
            // Press on empty canvas (the top-right corner: a compact canvas
            // starts at its top-left, so the boxes run along row 2 and the
            // loop lanes below them), drag, release.
            let c = v.canvas();
            let (x, y) = (c.x + c.width - 2, c.y);
            v.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), x, y));
            v.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), x - 5, y + 2));
            v.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), x - 5, y + 2));
            assert_ne!(v.pan(), pan, "locked={locked}");
            assert_eq!(
                v.selection(),
                Selection::Node("plan".into()),
                "a pan keeps the selection (locked={locked})"
            );
            // A compact canvas cannot zoom out: its boxes are one row tall.
            v.handle_mouse(mouse(MouseEventKind::ScrollDown, x, y));
            assert_eq!(v.zoom(), 1.0, "locked={locked}");
            v.tick(Duration::from_millis(100));
        }
        // A full canvas zooms at the wheel.
        let mut v = view();
        draw(&mut v, 220, 50);
        let c = v.canvas();
        let (x, y) = (c.x + c.width - 2, c.y + c.height - 2);
        let zoom = v.zoom();
        v.handle_mouse(mouse(MouseEventKind::ScrollDown, x, y));
        assert!(v.zoom() < zoom);
        v.handle_mouse(mouse(MouseEventKind::ScrollUp, x, y));
        assert!(v.zoom() > zoom * 0.9);
    }

    #[test]
    fn a_compact_canvas_starts_top_left_when_the_graph_overflows_and_fits_otherwise() {
        let mut v = FlowView::new(graph(), NodeStyle::Compact, true);
        draw(&mut v, 60, 12);
        assert_eq!(v.pan(), (1.0, 1.0), "the entry stage is on screen");
        assert_eq!(v.node_rect("plan").map(|r| r.0), Some(2));
        // Wide enough: centred like any other fit.
        let mut v = FlowView::new(graph(), NodeStyle::Compact, true);
        draw(&mut v, 200, 20);
        assert_ne!(v.pan(), (1.0, 1.0));
        assert_eq!(v.zoom(), 1.0);
        // A full canvas that overflows zooms out instead.
        let mut v = FlowView::new(graph(), NodeStyle::Full, false);
        draw(&mut v, 60, 20);
        assert!(v.zoom() < 1.0);
    }

    #[test]
    fn render_refits_when_the_area_changes_size() {
        let mut v = view();
        draw(&mut v, 220, 50);
        v.handle_key(KeyCode::Char('-'));
        let zoomed = v.zoom();
        // Same size: the user's zoom sticks.
        draw(&mut v, 220, 50);
        assert_eq!(v.zoom(), zoomed);
        // New size: fit again.
        draw(&mut v, 60, 20);
        assert_ne!(v.zoom(), zoomed);
    }
}
