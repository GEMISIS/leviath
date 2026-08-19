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
    Background, BackgroundVariant, Edge, FitViewOptions, Flow, FlowAction, Handle, HandlePosition,
    MiniMap, MiniMapPosition, Node, StepEdge, Theme, Viewport,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Block;

use crate::blueprint_edit::Positions;
use crate::tui::theme::C_ACCENT;

use super::content::{
    NODE_HEIGHT, NodeStatus, RunPhase, StageNodeContent, WorkerCounts, edge_style, node_width,
    palette,
};
use super::layout::{self, Direction, GraphLayout};
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

/// What the canvas did with the mouse that the editor has to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CanvasEvent {
    /// A handle was dragged onto another box: a path to add.
    Connected { from: String, to: String },
    /// A box was dropped somewhere new: positions to remember.
    Moved,
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
    layout: GraphLayout,
    edges: Vec<EdgeMeta>,
    locked: bool,
    /// An editor: handles show and connect, and the mouse's connections and
    /// drops are reported through [`FlowView::take_events`].
    edit: bool,
    /// Where dragged boxes were left (an editor); empty means the layered
    /// layout places every box.
    positions: Positions,
    events: Vec<CanvasEvent>,
    direction: Direction,
    /// Until the user turns the graph by hand, the first draw picks the
    /// direction that fits.
    auto_direction: bool,
    show_escape: bool,
    /// The whole graph, or (the default once a run is on the canvas) the
    /// path taken plus what can happen from here: the stages visited, the
    /// current one, the transitions between them, and the current stage's
    /// options with the stages they lead to. `t` toggles.
    show_all: bool,
    /// Transitions the run followed, from the last overlay.
    taken: HashSet<(String, String)>,
    /// The stage the run is in, from the last overlay.
    current: Option<String>,
    /// A stage to bring on screen at the next draw.
    reveal: Option<String>,
    last_current: Option<String>,
    /// The overlay last applied, re-applied after a rebuild.
    last_live: Option<LiveOverlay>,
    last_area: Rect,
    canvas: Rect,
    /// The longest lane a visible-by-default edge needs beside the nodes, so
    /// fitting the view leaves room for it. Escape lanes are not counted:
    /// they are hidden until asked for, and a long one would cost every fit
    /// its width.
    max_stem: f64,
}

/// Box size and gaps: `(node_w, node_h, gap_x, gap_y)`.
fn metrics(longest_id: usize) -> (f64, f64, f64, f64) {
    (node_width(longest_id), NODE_HEIGHT, 8.0, 1.0)
}

/// Put saved positions on the boxes, and any box without one to the right
/// of the rightmost saved box (the way The Lair appends a new stage), so an
/// arrangement survives a stage being added.
fn apply_positions(
    flow: &mut Flow<StageNodeContent, StepEdge>,
    positions: &Positions,
    node_w: f64,
    node_h: f64,
    gap_x: f64,
    gap_y: f64,
) {
    if positions.is_empty() {
        return;
    }
    let mut right = f64::MIN;
    let mut top = f64::MAX;
    for (x, y) in positions.values() {
        right = right.max(x + node_w);
        top = top.min(*y);
    }
    let ids: Vec<String> = flow.nodes().map(|n| n.id.clone()).collect();
    let mut appended = 0.0;
    for id in ids {
        match positions.get(&id) {
            Some((x, y)) => flow.set_node_position(&id, (*x, *y)),
            None => {
                flow.set_node_position(&id, (right + gap_x, top + appended));
                appended += node_h + gap_y;
            }
        }
    }
}

/// Which sides an edge leaves and enters on, and how far it travels before
/// turning, for a graph flowing in `direction`.
///
/// The main flow runs along the layer axis. Edges that go against it (a
/// loop back to an earlier layer, or out of a stage that only escapes reach)
/// leave and re-enter on the same side and run along a lane beside the
/// nodes: below them (right of them, top-to-bottom) for the flow's own
/// loops, above (left) for the escapes. A lane per layer of distance keeps a
/// long loop from overwriting a short one's label.
fn route(
    direction: Direction,
    class: EdgeClass,
    from_layer: usize,
    to_layer: usize,
) -> (HandlePosition, HandlePosition, bool, f64) {
    let loops_back = to_layer <= from_layer;
    let (forward_out, forward_in, loop_side, escape_side) = match direction {
        Direction::LeftToRight => (
            HandlePosition::Right,
            HandlePosition::Left,
            HandlePosition::Bottom,
            HandlePosition::Top,
        ),
        Direction::TopToBottom => (
            HandlePosition::Bottom,
            HandlePosition::Top,
            HandlePosition::Right,
            HandlePosition::Left,
        ),
    };
    let (src, tgt) = match class {
        EdgeClass::Escape => (escape_side, escape_side),
        _ if loops_back => (loop_side, loop_side),
        _ => (forward_out, forward_in),
    };
    let stem = if src == tgt {
        1.0 + from_layer.abs_diff(to_layer) as f64
    } else {
        1.0
    };
    (src, tgt, loops_back, stem)
}

impl std::fmt::Debug for FlowView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlowView")
            .field("entry", &self.graph.entry)
            .field("nodes", &self.graph.nodes.len())
            .field("direction", &self.direction)
            .field("show_escape", &self.show_escape)
            .field("show_all", &self.show_all)
            .finish()
    }
}

/// Handles on every side, named by side so an edge can pick where it
/// attaches. The forward sides (out along the flow, in against it) sit at
/// the middle of their edge; the lane sides carry the source a little past
/// the middle and the target a little before it, so a loop leaves and
/// re-enters at different cells. Hidden on a viewer, where handle glyphs
/// would read as ports; on an editor the flow-side pair shows and connects,
/// and the lanes stay hidden (routing, not ports).
fn handles(direction: Direction, edit: bool) -> Vec<Handle> {
    let (out, back, lane_a, lane_b) = match direction {
        Direction::LeftToRight => (
            HandlePosition::Right,
            HandlePosition::Left,
            HandlePosition::Bottom,
            HandlePosition::Top,
        ),
        Direction::TopToBottom => (
            HandlePosition::Bottom,
            HandlePosition::Top,
            HandlePosition::Right,
            HandlePosition::Left,
        ),
    };
    vec![
        Handle::source(out)
            .with_id(out.side_name())
            .with_hidden(!edit),
        Handle::source(lane_a)
            .with_id(lane_a.side_name())
            .with_offset(0.7)
            .with_connectable(false)
            .with_hidden(true),
        Handle::source(lane_b)
            .with_id(lane_b.side_name())
            .with_offset(0.7)
            .with_connectable(false)
            .with_hidden(true),
        Handle::target(back)
            .with_id(back.side_name())
            .with_hidden(!edit),
        Handle::target(lane_a)
            .with_id(lane_a.side_name())
            .with_offset(0.3)
            .with_connectable(false)
            .with_hidden(true),
        Handle::target(lane_b)
            .with_id(lane_b.side_name())
            .with_offset(0.3)
            .with_connectable(false)
            .with_hidden(true),
    ]
}

/// The canvas for `graph` laid out in `direction`: the nodes, the edges
/// remembered for live restyling, and the longest lane a visible edge needs.
fn build(
    graph: &StageGraph,
    layout: &GraphLayout,
    locked: bool,
    direction: Direction,
    edit: bool,
    positions: &Positions,
) -> (Flow<StageNodeContent, StepEdge>, Vec<EdgeMeta>, f64) {
    let longest = graph
        .nodes
        .iter()
        .map(|n| n.id.trim_start_matches("ext:").chars().count())
        .max()
        .unwrap_or(0);
    let (node_w, node_h, gap_x, gap_y) = metrics(longest);

    let nodes: Vec<Node<StageNodeContent>> = graph
        .nodes
        .iter()
        .map(|n| {
            Node::new(
                n.id.clone(),
                (0.0, 0.0),
                (node_w, node_h),
                StageNodeContent::from_node(n),
            )
            .with_handles(handles(direction, edit))
            // An external blueprint is drawn, not edited: nothing connects to
            // it and it cannot be moved.
            .with_connectable(edit && n.kind != super::model::NodeKind::ExternalBlueprint)
            .with_deletable(false)
            .with_resizable(false)
            .with_draggable(!locked && n.kind != super::model::NodeKind::ExternalBlueprint)
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
            let (src, tgt, loops_back, stem) = route(direction, e.class, from_layer, to_layer);
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
            let label = if edit {
                e.editor_label()
            } else {
                e.condition_label()
            };
            if !label.is_empty() {
                edge = edge.with_label(format!("[{label}]"));
            }
            edge
        })
        .collect();

    let mut flow = Flow::with_graph(nodes, edges)
        .expect("a StageGraph has unique ids, no self-loops and no dangling edges")
        .with_theme(Theme::Custom(palette()))
        // Zoomed out by hand a box still keeps its frame down to a couple
        // of rows; the fit itself never goes below 1.0.
        .with_min_zoom(0.5)
        .with_max_zoom(1.0)
        // Marching ants at a walking pace; the default is a sprint.
        .with_animation_speed(220)
        // A press on empty canvas is how a pan starts; it must not throw
        // the selection away, or Enter after a pan opens nothing.
        .with_deselect_on_pane_click(false)
        .with_locked(locked);
    flow.set_node_positions(layout.positions(direction, node_w, node_h, gap_x, gap_y));
    apply_positions(&mut flow, positions, node_w, node_h, gap_x, gap_y);
    if edit {
        // A path that exists is not drawn twice, whichever handles a drag
        // would route it through; and a box cannot be wired to itself on
        // the canvas (rataflow refuses), so a self-loop is added by key.
        let pairs: HashSet<(String, String)> = graph
            .edges
            .iter()
            .map(|e| (e.from.clone(), e.to.clone()))
            .collect();
        flow.set_connection_validator(move |c| {
            !pairs.contains(&(c.source.clone(), c.target.clone()))
        });
    }
    // No fit here: the first `render` settles the viewport for the area it
    // gets. A fit requested now would land after that and undo it.
    (flow, metas, max_stem)
}

impl FlowView {
    /// Build the canvas for `graph`. `locked` makes it a viewer: left-drag
    /// pans and nothing selects or moves, for the band and the preview; the
    /// explorer is unlocked, so boxes can be dragged into a better
    /// arrangement and clicked to select.
    pub(crate) fn new(graph: Arc<StageGraph>, locked: bool) -> Self {
        Self::build_view(graph, locked, false, Positions::new())
    }

    /// Build the canvas as an editor: handles show and connect, boxes drag,
    /// every edge is drawn, and `positions` (from an earlier session) place
    /// the boxes.
    pub(crate) fn new_editor(graph: Arc<StageGraph>, positions: Positions) -> Self {
        let mut view = Self::build_view(graph, false, true, positions);
        view.show_all = true;
        view.show_escape = true;
        view.sync_visibility();
        view
    }

    fn build_view(graph: Arc<StageGraph>, locked: bool, edit: bool, positions: Positions) -> Self {
        let layout = layout::layout(&graph);
        let (flow, edges, max_stem) = build(
            &graph,
            &layout,
            locked,
            Direction::LeftToRight,
            edit,
            &positions,
        );
        Self {
            flow,
            graph,
            layout,
            edges,
            locked,
            edit,
            positions,
            events: Vec::new(),
            direction: Direction::LeftToRight,
            auto_direction: true,
            show_escape: false,
            show_all: false,
            taken: HashSet::new(),
            current: None,
            reveal: None,
            last_current: None,
            last_live: None,
            last_area: Rect::default(),
            canvas: Rect::default(),
            max_stem,
        }
    }

    /// Lay the graph out the other way round. Dragged boxes go back to
    /// their computed places; the selection, the toggles and the run's
    /// overlay carry over.
    pub(crate) fn rotate(&mut self) {
        self.auto_direction = false;
        // Turning the graph is asking for the layout's arrangement.
        self.positions.clear();
        self.rebuild(self.direction.rotated());
    }

    /// Draw a different graph on the same canvas (the editor after an edit):
    /// selection by id, viewport, direction and toggles carry over;
    /// `positions` place the boxes.
    pub(crate) fn replace_graph(&mut self, graph: Arc<StageGraph>, positions: Positions) {
        self.graph = graph;
        self.layout = layout::layout(&self.graph);
        self.positions = positions;
        let area = self.last_area;
        self.rebuild(self.direction);
        // Same area, same camera: a rebuild here is an edit, not a resize.
        self.last_area = area;
    }

    /// Where every stage box sits now, for the layout store.
    pub(crate) fn positions(&self) -> Positions {
        self.flow
            .nodes()
            .filter(|n| !n.id.starts_with("ext:"))
            .map(|n| (n.id.clone(), (n.position.x, n.position.y)))
            .collect()
    }

    /// The mouse's connections and drops since the last call.
    pub(crate) fn take_events(&mut self) -> Vec<CanvasEvent> {
        std::mem::take(&mut self.events)
    }

    /// Mark a stage as named by the problems list, and as having its own
    /// context layout: the box shows a `!` and a `▣ own context` badge.
    pub(crate) fn set_flags(&mut self, id: &str, problem: bool, own_layout: bool) {
        if let Some(content) = self.flow.node_content_mut(id) {
            content.problem = problem;
            content.own_layout = own_layout;
        }
    }

    /// Bring a stage on screen at the next draw.
    pub(crate) fn reveal(&mut self, id: &str) {
        self.reveal = Some(id.to_string());
    }

    /// Nothing selected: the inspector falls back to the agent.
    pub(crate) fn clear_selection(&mut self) {
        self.flow.clear_selection();
    }

    /// Select the path from one stage to another.
    pub(crate) fn select_edge(&mut self, from: &str, to: &str) {
        if let Some(meta) = self
            .edges
            .iter()
            .find(|m| m.edge.from == from && m.edge.to == to)
        {
            let id = meta.id.clone();
            self.flow.clear_selection();
            self.flow.select_edge(&id);
        }
    }

    /// Which way the layers run.
    pub(crate) fn direction(&self) -> Direction {
        self.direction
    }

    fn rebuild(&mut self, direction: Direction) {
        let selected = self.flow.first_selected_node_id();
        let selected_edge = match self.selection() {
            Selection::Edge(e) => Some((e.from, e.to)),
            _ => None,
        };
        let viewport = self.flow.viewport;
        let (flow, edges, max_stem) = build(
            &self.graph,
            &self.layout,
            self.locked,
            direction,
            self.edit,
            &self.positions,
        );
        self.flow = flow;
        self.edges = edges;
        self.max_stem = max_stem;
        self.direction = direction;
        if let Some(live) = self.last_live.take() {
            self.apply_live(&live);
        }
        self.sync_visibility();
        if let Some(id) = selected {
            self.flow.select_node(&id);
        } else if let Some((from, to)) = selected_edge {
            self.select_edge(&from, &to);
        }
        // An editor keeps its camera across an edit; a rebuild from a turn
        // settles again for the current area at the next draw.
        if self.edit {
            self.flow.viewport = viewport;
        } else {
            self.last_area = Rect::default();
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

    /// The whole graph, or the run's path and options.
    pub(crate) fn show_all(&self) -> bool {
        self.show_all
    }

    /// Switch between the whole graph and the run's path and options.
    pub(crate) fn toggle_all(&mut self) {
        self.show_all = !self.show_all;
        self.sync_visibility();
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

    /// First look at a canvas of this size. Boxes are never shrunk to make
    /// the graph fit: a box zoomed down is an unreadable one. Instead the
    /// layers run the way that fits (left to right first, top to bottom when
    /// that overflows and the other does not); when neither fits the canvas
    /// starts at its top-left corner, entry stage on screen, and pans.
    fn settle(&mut self, area: Rect) {
        // Inside the block's border, less a cell of margin each side.
        let (inner_w, inner_h) = (f64::from(area.width) - 4.0, f64::from(area.height) - 4.0);
        let fits = |(w, h): (f64, f64)| w <= inner_w && h <= inner_h;
        let extent = |dir: Direction| {
            let longest = self
                .graph
                .nodes
                .iter()
                .map(|n| n.id.trim_start_matches("ext:").chars().count())
                .max()
                .unwrap_or(0);
            let (node_w, node_h, gap_x, gap_y) = metrics(longest);
            self.layout.extent(dir, node_w, node_h, gap_x, gap_y)
        };
        let other = self.direction.rotated();
        if self.auto_direction && !fits(extent(self.direction)) && fits(extent(other)) {
            self.rebuild(other);
        }
        if fits(self.world_extent()) {
            self.fit();
        } else {
            self.flow.viewport = Viewport {
                x: 1.0,
                y: 1.0,
                zoom: 1.0,
            };
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
            KeyCode::Char('r') => self.rotate(),
            KeyCode::Char('e') => self.toggle_escape(),
            KeyCode::Char('t') => self.toggle_all(),
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
            let response = self.flow.handle_mouse_event(event);
            if self.edit {
                for ev in response.events() {
                    match ev {
                        rataflow::FlowEvent::ConnectionCompleted(c) => {
                            self.events.push(CanvasEvent::Connected {
                                from: c.source.clone(),
                                to: c.target.clone(),
                            });
                        }
                        rataflow::FlowEvent::NodeDragEnded { .. } => {
                            self.events.push(CanvasEvent::Moved);
                        }
                        _ => {}
                    }
                }
            }
        }
        forward
    }

    /// Paint the run onto the blueprint. Cheap enough to call every draw:
    /// it mutates node content in place and never rebuilds the canvas.
    pub(crate) fn apply_live(&mut self, live: &LiveOverlay) {
        self.last_live = Some(live.clone());
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
        self.taken = live.taken.iter().cloned().collect();
        self.current = live.current.clone();
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

    /// Hidden nodes and edges follow the toggles. With a run on the canvas
    /// and `show_all` off, an edge shows when the run took it or can take it
    /// from where it is, and a node shows when the run has been in it, is in
    /// it, or can go to it next: no box without a line to it, no line into
    /// nothing. Escape edges hide with escapes off whatever else is on.
    fn sync_visibility(&mut self) {
        let focused = self.current.is_some() && !self.show_all;
        let current = self.current.clone().unwrap_or_default();
        let live_edge = |edge: &StageEdge| {
            self.taken.contains(&(edge.from.clone(), edge.to.clone())) || edge.from == current
        };
        let mut shown_nodes: HashSet<String> = HashSet::new();
        if focused {
            shown_nodes.insert(current.clone());
            for meta in &self.edges {
                let escape_hidden = meta.edge.class == EdgeClass::Escape && !self.show_escape;
                if live_edge(&meta.edge) && !escape_hidden {
                    shown_nodes.insert(meta.edge.from.clone());
                    shown_nodes.insert(meta.edge.to.clone());
                }
            }
            // A stage visited on the way, whichever edge brought the run.
            for node in self.flow.nodes() {
                if matches!(node.content.status, NodeStatus::Visited { .. }) {
                    shown_nodes.insert(node.id.clone());
                }
            }
        }
        let mut changed = false;
        for id in self.graph.ids() {
            let hide = focused && !shown_nodes.contains(id);
            changed |= self.flow.node(id).is_some_and(|n| n.hidden != hide);
            self.flow.set_node_hidden(id, hide);
        }
        // A different set of boxes is a different picture: fit it again at
        // the next draw (the fit follows the boxes on show).
        if changed {
            self.last_area = Rect::default();
        }
        for meta in &self.edges {
            let hide = (meta.edge.class == EdgeClass::Escape && !self.show_escape)
                || (focused && !live_edge(&meta.edge))
                || (focused
                    && !(shown_nodes.contains(&meta.edge.from)
                        && shown_nodes.contains(&meta.edge.to)));
            self.flow.set_edge_hidden(&meta.id, hide);
        }
    }

    /// Draw the canvas into `area`, inside `block`. Returns the canvas rect
    /// (the block's inside), which is what mouse routing hit-tests.
    pub(crate) fn render(&mut self, frame: &mut Frame, area: Rect, block: Block<'static>) -> Rect {
        let inner = block.inner(area);
        if (area.width, area.height) != (self.last_area.width, self.last_area.height) {
            self.last_area = area;
            self.settle(area);
        }
        // After settling: a settle may rebuild the canvas, and the block
        // belongs on the one that draws.
        self.flow.set_block(Some(block));
        if let Some(id) = self.reveal.take() {
            self.flow.ensure_node_visible(&id);
        }
        // A dotted grid under the graph says "canvas" and moves with a pan,
        // which is what makes a pan legible.
        frame.render_widget(
            Background::new(&self.flow)
                .variant(BackgroundVariant::Dots)
                .gap(8, 4),
            inner,
        );
        frame.render_widget(&mut self.flow, area);
        self.canvas = self.flow.canvas_area();
        // A minimap once the graph is bigger than the canvas and the canvas
        // has room for one.
        let (world_w, world_h) = self.world_extent();
        let overflows =
            world_w > f64::from(self.canvas.width) || world_h > f64::from(self.canvas.height);
        if overflows && self.canvas.width >= 60 && self.canvas.height >= 16 {
            frame.render_widget(
                MiniMap::new(&self.flow)
                    .position(MiniMapPosition::BottomRight)
                    .size(20, 6),
                self.canvas,
            );
        }
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
            .filter(|n| !n.hidden)
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
        .with_zoom_range(1.0, 1.0)
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
        FlowView::new(graph(), false)
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
        // No run on the canvas: `t` flips the flag and nothing hides either
        // way, because there is no path to focus on.
        assert!(!v.show_all());
        assert!(v.handle_key(KeyCode::Char('t')));
        assert!(v.show_all());
        assert!(!v.node_hidden("plan") && !v.edge_hidden("plan", "implement"));
        assert!(v.handle_key(KeyCode::Char('t')));
        assert!(!v.node_hidden("plan") && !v.edge_hidden("plan", "implement"));
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
        // With a run on the canvas the picture is the path and the options:
        // the stages visited, the current one, what it can go to next, and
        // the edges between them. The rest goes until `t`.
        assert!(!v.edge_hidden("plan", "implement"), "taken");
        assert!(
            !v.edge_hidden("review", "done"),
            "an option from the current stage"
        );
        assert!(!v.node_hidden("done"), "where an option leads");
        assert!(
            v.edge_hidden("recover", "plan"),
            "neither taken nor available"
        );
        assert!(v.node_hidden("recover"), "no line to it, no box");
        // Escapes stay off even in focus: the escape into recover does not
        // bring the box back...
        assert!(v.edge_hidden("implement", "recover"));
        // ...and turning escapes on does not either, because implement is
        // not where the run is: in focus, `e` shows the escapes from here.
        v.toggle_escape();
        assert!(v.edge_hidden("implement", "recover"), "not on the path");
        assert!(v.node_hidden("recover"));
        v.toggle_escape();
        assert!(!v.show_all());
        assert!(v.handle_key(KeyCode::Char('t')));
        assert!(v.show_all());
        assert!(!v.edge_hidden("recover", "plan"));
        assert!(!v.node_hidden("recover"));
        v.toggle_all();
        assert!(v.edge_hidden("recover", "plan"));
        assert!(v.node_hidden("recover"));
        // A non-fan-out current stage never shows worker counts.
        let (_, text) = draw(&mut v, 220, 50);
        assert!(!text.contains(" run ·"), "{text}");
        assert!(text.contains("iter 2"), "{text}");
        assert!(text.contains("10:00:00"), "{text}");
        assert!(text.contains("implement ×2"), "{text}");

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
        let mut v = FlowView::new(g, false);
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
            let mut v = FlowView::new(graph(), locked);
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
            // The wheel zooms, locked or not.
            v.handle_mouse(mouse(MouseEventKind::ScrollDown, x, y));
            assert!(v.zoom() < 1.0, "locked={locked}");
            v.tick(Duration::from_millis(100));
        }
        // And zooms back in.
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
    fn a_canvas_the_graph_overflows_starts_top_left_and_boxes_are_never_shrunk() {
        let mut v = FlowView::new(graph(), true);
        draw(&mut v, 60, 12);
        assert_eq!(v.pan(), (1.0, 1.0), "the entry stage is on screen");
        assert_eq!(v.node_rect("plan").map(|r| r.0), Some(2));
        assert_eq!(v.direction(), Direction::LeftToRight);
        // Wide enough: centred like any other fit.
        let mut v = FlowView::new(graph(), true);
        draw(&mut v, 200, 20);
        assert_ne!(v.pan(), (1.0, 1.0));
        assert_eq!(v.zoom(), 1.0);
        // A full canvas that overflows both ways keeps its boxes whole and
        // starts at the corner too, rather than shrinking them to fit.
        let mut v = FlowView::new(graph(), false);
        draw(&mut v, 60, 20);
        assert_eq!(v.zoom(), 1.0);
        assert_eq!(v.pan(), (1.0, 1.0));
        assert!(format!("{v:?}").contains("LeftToRight"));
    }

    #[test]
    fn a_tall_narrow_canvas_turns_the_graph_top_to_bottom_and_r_turns_it_back() {
        // Five stages of full boxes: 172 cells wide left to right, 24 rows
        // top to bottom. A 70x40 canvas fits only the second.
        let mut v = FlowView::new(graph(), false);
        v.select_stage("review");
        v.toggle_escape();
        draw(&mut v, 70, 40);
        assert_eq!(v.direction(), Direction::TopToBottom);
        assert_eq!(v.zoom(), 1.0);
        // The turn kept the selection and the toggles.
        assert_eq!(v.selection(), Selection::Node("review".into()));
        assert!(v.show_escape());
        assert!(!v.edge_hidden("implement", "recover"));
        // Layers are rows now: implement sits under plan, not beside it.
        let plan = v.node_rect("plan").unwrap();
        let implement = v.node_rect("implement").unwrap();
        assert_eq!(plan.0, implement.0);
        assert!(implement.1 > plan.3);
        // `r` turns it back by hand, and again.
        assert!(v.handle_key(KeyCode::Char('r')));
        assert_eq!(v.direction(), Direction::LeftToRight);
        draw(&mut v, 70, 40);
        let plan = v.node_rect("plan").unwrap();
        let implement = v.node_rect("implement").unwrap();
        assert_eq!(plan.1, implement.1);
        assert!(v.handle_key(KeyCode::Char('r')));
        assert_eq!(v.direction(), Direction::TopToBottom);
        // The live overlay survives a turn.
        v.apply_live(&LiveOverlay {
            current: Some("plan".into()),
            run: Some(RunPhase::Active),
            ..LiveOverlay::default()
        });
        v.rotate();
        assert_eq!(
            v.node_status("plan"),
            Some(NodeStatus::Current {
                run: RunPhase::Active,
                times: 1
            })
        );
        // Boxes can be dragged on an unlocked canvas: press on one, drag,
        // and it has moved.
        draw(&mut v, 200, 50);
        let (x, y, _, _) = v.node_rect("plan").unwrap();
        let (x, y) = (x as u16 + 2, y as u16 + 1);
        v.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), x, y));
        v.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), x + 6, y + 3));
        v.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), x + 6, y + 3));
        draw(&mut v, 200, 50);
        let moved = v.node_rect("plan").unwrap();
        assert!(moved.0 > x as i32 - 2 + 3, "{moved:?}");
        // A minimap appears when the graph is bigger than the canvas.
        let mut v = FlowView::new(graph(), false);
        let (_, text) = draw(&mut v, 90, 24);
        assert!(text.contains('▄'), "{text}");
        // And not when everything is on screen already.
        let mut v = FlowView::new(graph(), false);
        let (_, text) = draw(&mut v, 220, 50);
        assert!(!text.contains('▄'), "{text}");
        // Turning a canvas with nothing selected selects nothing after.
        v.rotate();
        assert_eq!(v.selection(), Selection::Nothing);
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

    #[test]
    fn an_editor_shows_every_edge_connects_by_mouse_and_reports_it() {
        let mut v = FlowView::new_editor(graph(), Positions::new());
        assert!(v.show_all() && v.show_escape());
        assert!(
            !v.edge_hidden("implement", "recover"),
            "escapes show on an editor"
        );
        let (_, text) = draw(&mut v, 220, 50);
        assert!(
            text.contains("[hint]") || text.contains("[always]"),
            "{text}"
        );
        assert!(text.contains("●"), "handles show: {text}");
        // Drag from done's source handle onto plan's target handle: a new
        // connection, reported once and not drawn (the editor adds it).
        let (_, dy, dr, db) = v.node_rect("done").expect("drawn");
        let (px, py, _, pb) = v.node_rect("plan").expect("drawn");
        let from = ((dr - 1) as u16, ((dy + db) / 2) as u16);
        let to = (px as u16, ((py + pb) / 2) as u16);
        v.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            from.0,
            from.1,
        ));
        v.handle_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            to.0 + 5,
            to.1,
        ));
        v.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), to.0, to.1));
        v.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), to.0, to.1));
        assert_eq!(
            v.take_events(),
            vec![CanvasEvent::Connected {
                from: "done".into(),
                to: "plan".into()
            }]
        );
        assert!(v.take_events().is_empty(), "taken once");
        // A pair that already exists is refused by the validator: no event.
        let (_, py2, pr, pb2) = v.node_rect("plan").expect("drawn");
        let (ix, iy, _, ib) = v.node_rect("implement").expect("drawn");
        let from = ((pr - 1) as u16, ((py2 + pb2) / 2) as u16);
        let to = (ix as u16, ((iy + ib) / 2) as u16);
        v.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            from.0,
            from.1,
        ));
        v.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), to.0, to.1));
        v.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), to.0, to.1));
        assert!(v.take_events().is_empty(), "plan → implement exists");
        // Dragging a box reports a move, and the positions read back.
        let (x, y, _, _) = v.node_rect("review").expect("drawn");
        let (x, y) = (x as u16 + 2, y as u16 + 1);
        v.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), x, y));
        v.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), x + 8, y + 4));
        v.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), x + 8, y + 4));
        assert_eq!(v.take_events(), vec![CanvasEvent::Moved]);
        let positions = v.positions();
        assert_eq!(positions.len(), 5, "every stage, no ext: nodes");
        // Flags and selection helpers.
        v.set_flags("plan", true, true);
        v.set_flags("ghost", true, true);
        v.select_stage("plan");
        let (_, text) = draw(&mut v, 220, 50);
        assert!(text.contains("! ○ plan"), "{text}");
        assert!(text.contains("▣ own context"), "{text}");
        v.clear_selection();
        assert_eq!(v.selection(), Selection::Nothing);
        let plan_implement = v
            .graph()
            .edges
            .iter()
            .find(|e| e.from == "plan" && e.to == "implement")
            .cloned()
            .expect("in the graph");
        v.select_edge("plan", "implement");
        assert_eq!(v.selection(), Selection::Edge(plan_implement.clone()));
        v.select_edge("plan", "nowhere");
        assert_eq!(
            v.selection(),
            Selection::Edge(plan_implement.clone()),
            "an unknown path leaves the selection"
        );
        v.reveal("done");
        draw(&mut v, 220, 50);
        // Replace the graph: the moved box keeps its place, the camera stays,
        // the selected edge is selected again.
        let camera = v.pan();
        let mut positions = v.positions();
        let review = positions["review"];
        // A stage the positions do not know lands right of the rightmost.
        positions.remove("done");
        v.replace_graph(graph(), positions.clone());
        assert_eq!(v.positions()["review"], review);
        assert_eq!(v.pan(), camera);
        assert_eq!(v.selection(), Selection::Edge(plan_implement));
        let done = v.positions()["done"];
        assert!(
            done.0 > review.0,
            "appended to the right: {done:?} vs {review:?}"
        );
        // Turning the graph forgets the arrangement.
        v.rotate();
        assert_ne!(v.positions()["review"], review);
        // A rebuild with a stage node selected keeps it.
        v.select_stage("plan");
        v.replace_graph(graph(), Positions::new());
        assert_eq!(v.selection(), Selection::Node("plan".into()));
    }
}
