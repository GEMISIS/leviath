//! What a stage looks like on the canvas: the node box, the edge strokes,
//! and the palette that ties them to the rest of the TUI.
//!
//! Nodes are drawn with ordinary ratatui widgets through rataflow's
//! [`NodeContent`] trait; edges use rataflow's built-in `StepEdge` with a
//! per-class [`EdgeStyle`], so there is nothing edge-shaped to test beyond
//! [`edge_style`].

use rataflow::{EdgeMarker, EdgeStyle, NodeContent, NodeRenderContext, Palette};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget};

use crate::tui::theme::*;

use super::model::{EdgeClass, NodeKind, StageNode};

/// The run's state, as far as the node it is in cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunPhase {
    Active,
    Waiting,
    Paused,
    Stale,
    Complete,
    Error,
    Cancelled,
}

impl RunPhase {
    fn colour(self) -> Color {
        match self {
            RunPhase::Active => C_ACTIVE,
            RunPhase::Waiting | RunPhase::Paused | RunPhase::Stale => C_WARN,
            RunPhase::Complete => C_SUCCESS,
            RunPhase::Error => C_ERROR,
            RunPhase::Cancelled => C_DIM,
        }
    }

    fn glyph(self, tick: u64) -> &'static str {
        match self {
            RunPhase::Active => SPINNER[(tick as usize) % SPINNER.len()],
            RunPhase::Waiting => GLYPH_WAITING,
            RunPhase::Paused => GLYPH_PENDING,
            RunPhase::Stale | RunPhase::Error => GLYPH_ERROR,
            RunPhase::Complete => GLYPH_COMPLETE,
            RunPhase::Cancelled => "⊘",
        }
    }

    /// Whether the run has stopped for good, so nothing about it can move
    /// again.
    ///
    /// The animated edge means "the run is travelling this path". On a run
    /// that has finished it was still pulsing into the last node it reached,
    /// which reads as a run still going - the one thing the graph is there to
    /// tell you at a glance. A parked run (waiting, paused, idle, stale) keeps
    /// the animation: it has not finished, and the pulse is what says where it
    /// stopped.
    pub(crate) fn is_finished(self) -> bool {
        matches!(
            self,
            RunPhase::Complete | RunPhase::Error | RunPhase::Cancelled
        )
    }

    /// The word the node shows for the phase; the spinner says "running"
    /// on its own, so `Active` has none.
    fn word(self) -> Option<&'static str> {
        match self {
            RunPhase::Active => None,
            RunPhase::Waiting => Some("waiting"),
            RunPhase::Paused => Some("paused"),
            RunPhase::Stale => Some("stale"),
            RunPhase::Complete => Some("complete"),
            RunPhase::Error => Some("error"),
            RunPhase::Cancelled => Some("cancelled"),
        }
    }
}

/// Where a run is relative to one stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeStatus {
    /// Never entered.
    Pending,
    /// Entered before, not the current stage.
    Visited { times: usize, errored: bool },
    /// The stage the run is in now.
    Current { run: RunPhase, times: usize },
}

impl NodeStatus {
    /// Glyph and colour for the title, given the animation tick.
    fn look(self, tick: u64) -> (&'static str, Color) {
        match self {
            NodeStatus::Pending => (GLYPH_PENDING, C_DIM),
            NodeStatus::Visited { errored: true, .. } => (GLYPH_ERROR, C_ERROR),
            NodeStatus::Visited { errored: false, .. } => (GLYPH_COMPLETE, C_WHITE),
            NodeStatus::Current { run, .. } => (run.glyph(tick), run.colour()),
        }
    }

    fn times(self) -> usize {
        match self {
            NodeStatus::Pending => 0,
            NodeStatus::Visited { times, .. } | NodeStatus::Current { times, .. } => times,
        }
    }
}

/// A box's height on the canvas: a border, the title, two detail rows.
pub(crate) const NODE_HEIGHT: f64 = 4.0;

/// The smallest a box is drawn, so a one-word stage still reads as a box and
/// the worker-count detail row (`⑂ 3 run · 1 done · 0 fail`) has room.
pub(crate) const MIN_NODE_WIDTH: usize = 28;

/// A box's width on the canvas, from the cells its content needs
/// ([`StageNodeContent::box_width`]), floored at [`MIN_NODE_WIDTH`]. A
/// left-to-right blueprint gives each box its own width this way; the snake and
/// the top-to-bottom layout share one width (the widest node) so their columns
/// stay aligned.
pub(crate) fn node_width(content_cells: usize) -> f64 {
    content_cells.max(MIN_NODE_WIDTH) as f64
}

/// Live counts of a fan-out stage's workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct WorkerCounts {
    pub(crate) running: usize,
    pub(crate) done: usize,
    pub(crate) failed: usize,
}

/// One node's content: the static shape from the blueprint plus whatever the
/// run has done to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StageNodeContent {
    pub(crate) name: String,
    pub(crate) kind_label: &'static str,
    pub(crate) is_external: bool,
    pub(crate) is_entry: bool,
    pub(crate) is_terminal: bool,
    pub(crate) self_loop: bool,
    /// Mime the stage takes beyond text, as patterns (`image/*`).
    pub(crate) inputs: Vec<String>,
    /// The types of the files it declares it hands back.
    pub(crate) outputs: Vec<String>,
    // ── editor ──
    /// The editor's problems list names this stage.
    pub(crate) problem: bool,
    /// The stage has a context layout of its own.
    pub(crate) own_layout: bool,
    // ── live ──
    pub(crate) status: NodeStatus,
    /// The run's iteration count, when this is the current stage. It counts
    /// the whole run, so it is not shown against the stage's own ceiling.
    pub(crate) iteration: Option<usize>,
    /// When the stage was last entered, as `HH:MM:SS`.
    pub(crate) last_seen: Option<String>,
    /// Workers of a fan-out stage that is running now.
    pub(crate) workers: Option<WorkerCounts>,
    pub(crate) tick: u64,
}

impl StageNodeContent {
    /// The blueprint's view of a node, before any run touched it.
    pub(crate) fn from_node(node: &StageNode) -> Self {
        Self {
            name: node.id.trim_start_matches("ext:").to_string(),
            kind_label: node.kind_label(),
            is_external: node.kind == NodeKind::ExternalBlueprint,
            is_entry: node.is_entry,
            is_terminal: node.is_terminal || node.allow_complete,
            self_loop: node.self_loop,
            inputs: node.inputs.clone(),
            outputs: node.outputs.clone(),
            problem: false,
            own_layout: false,
            status: NodeStatus::Pending,
            iteration: None,
            last_seen: None,
            workers: None,
            tick: 0,
        }
    }

    /// Reset the live part, keeping the blueprint part.
    pub(crate) fn clear_live(&mut self) {
        self.status = NodeStatus::Pending;
        self.iteration = None;
        self.last_seen = None;
        self.workers = None;
    }

    /// The title: entry marker, status glyph, name, visit count. `budget` is
    /// how many cells it may take (padding included); the name gives way
    /// first, with an ellipsis, so the count and the closing bracket or
    /// border stay in view.
    fn title(&self, selected: bool, budget: usize) -> Line<'static> {
        let (glyph, colour) = self.status.look(self.tick);
        let mut style = Style::default().fg(colour);
        if selected || matches!(self.status, NodeStatus::Current { .. }) {
            style = style.add_modifier(Modifier::BOLD);
        }
        let mut prefix = String::new();
        if self.is_entry {
            prefix.push_str("▶ ");
        }
        if self.problem {
            prefix.push_str("! ");
        }
        prefix.push_str(glyph);
        prefix.push(' ');
        let times = self.status.times();
        let suffix = if times > 1 {
            format!(" ×{times}")
        } else {
            String::new()
        };
        // Two cells of padding around the text.
        let fixed = 2 + prefix.chars().count() + suffix.chars().count();
        let room = budget.saturating_sub(fixed);
        let name = fit(&self.name, room);
        Line::from(Span::styled(format!(" {prefix}{name}{suffix} "), style))
    }

    /// The first detail row: what the stage is and where the run is with it.
    /// A stage is only ever current (the phase and iteration matter) or
    /// visited (when it last ran matters) or pending (only its kind), never
    /// two at once, so the row never has to carry an iteration and a clock
    /// together and stays short enough for a plain box.
    fn detail_row(&self) -> String {
        if let Some(w) = self.workers {
            return format!("⑂ {} run · {} done · {} fail", w.running, w.done, w.failed);
        }
        match self.status {
            NodeStatus::Current { run, .. } => {
                // The phase takes the kind label's place: "waiting" or the
                // spinner's silence is what matters on the stage the run is in.
                let mut parts = vec![run.word().unwrap_or(self.kind_label).to_string()];
                if let Some(iteration) = self.iteration {
                    parts.push(format!("iter {iteration}"));
                }
                parts.join(" · ")
            }
            NodeStatus::Visited { .. } => self
                .last_seen
                .clone()
                .unwrap_or_else(|| self.kind_label.to_string()),
            NodeStatus::Pending => self.kind_label.to_string(),
        }
    }

    /// The second detail row: what the stage takes and hands back, read as
    /// `in <types> · out <types>`. Every stage takes and returns text, so
    /// `text` leads both lists and the stage's own mime types (an image it
    /// draws, a document it hands back) follow. `↺ loops` and `▣ own` ride
    /// the front of the row. It is static - no live decoration - so a box can
    /// be sized to it once and never have to truncate it. `⏹ can end` is not
    /// here either: it is a corner marker on the border (see
    /// [`Self::end_marker`]).
    fn io_row(&self) -> String {
        let listed = |extra: &[String]| {
            std::iter::once("text".to_string())
                .chain(extra.iter().cloned())
                .collect::<Vec<_>>()
                .join(" · ")
        };
        let mut parts: Vec<String> = Vec::new();
        if self.self_loop {
            parts.push("↺ loops".to_string());
        }
        if self.own_layout {
            parts.push("▣ own".to_string());
        }
        parts.push(format!("in {}", listed(&self.inputs)));
        parts.push(format!("out {}", listed(&self.outputs)));
        parts.join(" · ")
    }

    /// The corner marker for a stage the run can end at, or `None`. Drawn
    /// right-aligned on the top border rather than in a row, so "this can be
    /// the last stage" reads as a property of the box, not another badge
    /// competing with the mime types below.
    fn end_marker(&self) -> Option<&'static str> {
        self.is_terminal.then_some("⏹")
    }

    /// The box width this node needs, in cells: the widest of its three rows
    /// (title, mode, in/out) plus the borders. The name is measured whole
    /// here - the box is sized to show it - even though [`Self::title`]
    /// ellipsizes it when a box is later squeezed narrower than this.
    pub(crate) fn box_width(&self) -> usize {
        let title = self.title_cells();
        // The in/out row is static and the mode row is short in every state
        // (phase and iteration, or a clock, never both), so a box sized before
        // the run starts still fits every row once it is under way. A live
        // "iter N" grows the mode row a little; the breathing cell absorbs it.
        let detail = 1 + self.detail_row().chars().count();
        let io = 1 + self.io_row().chars().count();
        // Two borders, and a cell of breathing room on the right.
        title.max(detail).max(io) + 3
    }

    /// The cells [`Self::title`] wants with the name shown whole and the end
    /// marker's corner left free.
    fn title_cells(&self) -> usize {
        let mut prefix = 0;
        if self.is_entry {
            prefix += 2;
        }
        if self.problem {
            prefix += 2;
        }
        // Status glyph and its trailing space.
        prefix += 2;
        let times = self.status.times();
        let suffix = if times > 1 {
            format!(" ×{times}").chars().count()
        } else {
            0
        };
        // Two cells of padding around the text, and the corner marker plus a
        // gap when the stage can end there.
        let marker = if self.end_marker().is_some() { 2 } else { 0 };
        2 + prefix + self.name.chars().count() + suffix + marker
    }
}

/// `text`, cut to `room` cells with an ellipsis when it does not fit.
fn fit(text: &str, room: usize) -> String {
    if text.chars().count() <= room {
        return text.to_string();
    }
    let keep = room.saturating_sub(1);
    let mut cut: String = text.chars().take(keep).collect();
    if room > 0 {
        cut.push('…');
    }
    cut
}

impl NodeContent for StageNodeContent {
    fn render(&self, ctx: &NodeRenderContext, buf: &mut Buffer) {
        let area = ctx.area;
        let (_, colour) = self.status.look(self.tick);
        // Selection is the border's job: a thick focus-coloured frame (or
        // bright brackets), so the title itself keeps its status colour and
        // nothing on it flips to reversed video.
        let border_style = if ctx.selected {
            Style::default()
                .fg(C_BORDER_FOCUS)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(colour)
        };
        let width = area.width as usize;
        // A box zoomed down to a row: brackets are the bounds.
        if area.height < 2 {
            let mut line = self.title(ctx.selected, width.saturating_sub(2));
            line.spans.insert(0, Span::styled("[", border_style));
            line.spans.push(Span::styled("]", border_style));
            Paragraph::new(line).render(area, buf);
            return;
        }
        let border_type = match (ctx.selected, self.is_external) {
            (true, _) => BorderType::Thick,
            (false, true) => BorderType::Double,
            (false, false) => BorderType::Rounded,
        };
        // The end marker sits in the top-right corner, so the left title is
        // given two fewer cells when one is present to keep them apart.
        let marker = self.end_marker();
        let title_budget = width.saturating_sub(if marker.is_some() { 4 } else { 2 });
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_type(border_type)
            .border_style(border_style)
            .title(self.title(ctx.selected, title_budget));
        if let Some(marker) = marker {
            block = block.title_top(
                Line::from(Span::styled(marker, Style::default().fg(C_MUTED))).right_aligned(),
            );
        }
        let inner = block.inner(area);
        block.render(area, buf);
        let rows = [
            Line::from(Span::styled(
                format!(" {}", self.detail_row()),
                Style::default().fg(C_MUTED),
            )),
            Line::from(Span::styled(
                format!(" {}", self.io_row()),
                Style::default().fg(C_DIM),
            )),
        ];
        let inner = Rect {
            height: inner.height.min(rows.len() as u16),
            ..inner
        };
        Paragraph::new(rows.to_vec()).render(inner, buf);
    }
}

/// The stroke for an edge of `class`. `taken` marks an edge the run has
/// actually followed.
pub(crate) fn edge_style(class: EdgeClass, back_edge: bool, taken: bool) -> EdgeStyle {
    let colour = match (class, back_edge, taken) {
        (_, _, true) => C_WHITE,
        (EdgeClass::Escape, _, _) => C_DIM,
        (_, true, _) => C_WARN,
        (EdgeClass::FanOut, _, _) => C_ACCENT,
        (EdgeClass::Primary, _, _) => C_MUTED,
    };
    let base = match class {
        EdgeClass::Escape => EdgeStyle::dotted(),
        EdgeClass::Primary if back_edge => EdgeStyle::dotted(),
        EdgeClass::FanOut => EdgeStyle::default()
            .with_line_chars('═', '║')
            .with_corner_chars(['╔', '╗', '╚', '╝'])
            .with_marker_end(EdgeMarker::Diamond),
        EdgeClass::Primary => EdgeStyle::default(),
    };
    base.with_stroke_style(Style::default().fg(colour))
        .with_label_style(Style::default().fg(colour))
}

/// rataflow's semantic palette, in the dashboard's colours.
pub(crate) fn palette() -> Palette {
    Palette {
        canvas_bg: Color::Reset,
        surface: Color::Reset,
        muted: C_MUTED,
        subtle: C_DIM,
        accent: C_ACCENT,
        text: C_WHITE,
        success: C_SUCCESS,
        error: C_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::{NodeKind, StageKind, WorkerRef};
    use super::*;
    use rataflow::{Position, Theme};

    fn node(id: &str, kind: NodeKind) -> StageNode {
        StageNode {
            outputs: Vec::new(),
            inputs: Vec::new(),
            id: id.to_string(),
            kind,
            is_entry: false,
            is_terminal: false,
            allow_complete: false,
            self_loop: false,
            max_iterations: None,
            max_revisits: None,
            description: None,
        }
    }

    fn content() -> StageNodeContent {
        StageNodeContent::from_node(&node("plan", NodeKind::Stage(StageKind::Autonomous)))
    }

    fn draw(content: &StageNodeContent, area: Rect, selected: bool) -> (Buffer, String) {
        let mut buf = Buffer::empty(area);
        let ctx = NodeRenderContext {
            id: "plan",
            area,
            selected,
            dragging: false,
            position_absolute: Position::new(0.0, 0.0),
            theme: Theme::Custom(palette()),
            animation_phase: 0,
        };
        content.render(&ctx, &mut buf);
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        (buf, text)
    }

    /// The style of the first cell holding `needle`'s first character.
    fn style_at(buf: &Buffer, needle: &str) -> Style {
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        let chars: Vec<char> = text.chars().collect();
        let needle_chars: Vec<char> = needle.chars().collect();
        let missing = format!("{needle:?} in {text:?}");
        let idx = (0..chars.len())
            .find(|&i| chars[i..].starts_with(&needle_chars))
            .expect(&missing);
        let x = (idx % buf.area.width as usize) as u16;
        let y = (idx / buf.area.width as usize) as u16;
        buf.cell((x + buf.area.x, y + buf.area.y)).unwrap().style()
    }

    #[test]
    fn pending_visited_and_every_current_run_phase_render_their_glyph_and_colour() {
        let area = Rect::new(0, 0, 40, 4);
        let mut c = content();
        let (buf, text) = draw(&c, area, false);
        assert!(text.contains(&format!("{GLYPH_PENDING} plan")), "{text}");
        assert_eq!(style_at(&buf, "plan").fg, Some(C_DIM));
        assert!(text.contains("autonomous"), "{text}");

        c.status = NodeStatus::Visited {
            times: 2,
            errored: false,
        };
        let (buf, text) = draw(&c, area, false);
        assert!(
            text.contains(&format!("{GLYPH_COMPLETE} plan ×2")),
            "{text}"
        );
        assert_eq!(style_at(&buf, "plan").fg, Some(C_WHITE));

        c.status = NodeStatus::Visited {
            times: 1,
            errored: true,
        };
        let (buf, text) = draw(&c, area, false);
        assert!(text.contains(&format!("{GLYPH_ERROR} plan")), "{text}");
        assert!(!text.contains("×1"), "a single visit has no count: {text}");
        assert_eq!(style_at(&buf, "plan").fg, Some(C_ERROR));

        let phases = [
            (
                RunPhase::Active,
                SPINNER[3],
                C_ACTIVE,
                "autonomous · iter 3",
            ),
            (RunPhase::Waiting, GLYPH_WAITING, C_WARN, "waiting · iter 3"),
            (RunPhase::Paused, GLYPH_PENDING, C_WARN, "paused"),
            (RunPhase::Stale, GLYPH_ERROR, C_WARN, "stale"),
            (RunPhase::Complete, GLYPH_COMPLETE, C_SUCCESS, "complete"),
            (RunPhase::Error, GLYPH_ERROR, C_ERROR, "error"),
            (RunPhase::Cancelled, "⊘", C_DIM, "cancelled"),
        ];
        c.tick = 3;
        c.iteration = Some(3);
        for (run, glyph, colour, word) in phases {
            c.status = NodeStatus::Current { run, times: 1 };
            let (buf, text) = draw(&c, area, false);
            assert!(text.contains(&format!("{glyph} plan")), "{run:?}: {text}");
            assert!(text.contains(word), "{run:?}: {text}");
            let style = style_at(&buf, "plan");
            assert_eq!(style.fg, Some(colour), "{run:?}");
            assert!(style.add_modifier.contains(Modifier::BOLD), "{run:?}");
        }
        c.status = NodeStatus::Current {
            run: RunPhase::Active,
            times: 3,
        };
        let (_, text) = draw(&c, area, false);
        assert!(text.contains("iter 3"), "{text}");
        assert!(
            !text.contains("iter 3/"),
            "the ceiling is per stage, the count per run: {text}"
        );
        assert!(text.contains("×3"), "{text}");
    }

    #[test]
    fn badges_workers_entry_terminal_external_and_last_seen_render() {
        let mut n = node(
            "split",
            NodeKind::Stage(StageKind::FanOut {
                worker: WorkerRef::Agent("researcher".into()),
                merge: None,
                max_workers: 4,
            }),
        );
        n.is_entry = true;
        n.self_loop = true;
        n.allow_complete = true;
        let mut c = StageNodeContent::from_node(&n);
        assert_eq!(c.kind_label, "fan-out");
        c.last_seen = Some("14:22:01".to_string());
        c.workers = Some(WorkerCounts {
            running: 3,
            done: 2,
            failed: 1,
        });
        let (_, text) = draw(&c, Rect::new(0, 0, 60, 4), false);
        assert!(text.contains("▶"), "entry marker: {text}");
        // A running fan-out shows its worker counts on the mode row.
        assert!(text.contains("⑂ 3 run · 2 done · 1 fail"), "{text}");
        // "can end" is a corner marker on the border now, not a badge.
        assert!(text.contains("⏹"), "can-end corner marker: {text}");
        assert!(
            !text.contains("can end"),
            "no can-end badge in a row: {text}"
        );
        // Every stage takes and hands back text; loops leads the in/out row.
        assert!(text.contains("↺ loops · in text · out text"), "{text}");
        // The stage's own mime types join text in the in/out lists.
        c.inputs = vec!["image/*".to_string(), "audio/wav".to_string()];
        c.outputs = vec!["video/mp4".to_string()];
        let (_, text) = draw(&c, Rect::new(0, 0, 80, 4), false);
        assert!(
            text.contains("in text · image/* · audio/wav · out text · video/mp4"),
            "{text}"
        );
        // The last-seen clock rides the mode row of a visited stage (not a
        // running one, whose row shows its workers).
        c.workers = None;
        c.status = NodeStatus::Visited {
            times: 1,
            errored: false,
        };
        let (_, text) = draw(&c, Rect::new(0, 0, 80, 4), false);
        assert!(
            text.contains("14:22:01"),
            "last-seen on a visited stage: {text}"
        );
        c.clear_live();
        assert_eq!(c.status, NodeStatus::Pending);
        assert!(c.workers.is_none() && c.last_seen.is_none() && c.iteration.is_none());

        let ext = StageNodeContent::from_node(&node("ext:researcher", NodeKind::ExternalBlueprint));
        assert!(ext.is_external);
        assert_eq!(ext.name, "researcher");
        let (_, text) = draw(&ext, Rect::new(0, 0, 24, 4), false);
        assert!(text.contains("blueprint"), "{text}");
        assert!(
            text.contains('╔'),
            "double border for an external node: {text}"
        );
    }

    #[test]
    fn the_end_marker_sits_in_the_corner_and_the_io_row_names_text_both_ways() {
        let mut n = node("describe", NodeKind::Stage(StageKind::Autonomous));
        n.is_terminal = true;
        n.inputs = vec!["image/*".to_string()];
        n.outputs = vec!["image/*".to_string()];
        let c = StageNodeContent::from_node(&n);
        let width = c.box_width();
        let (_, text) = draw(&c, Rect::new(0, 0, width as u16, 4), false);
        // The can-end marker is on the top border, in the right corner.
        let marker = text.chars().position(|ch| ch == '⏹').expect("a marker");
        assert_eq!(marker / width, 0, "on the top border: {text}");
        assert!(marker % width >= width - 3, "in the right corner: {text}");
        // The in/out row leads each side with text and lists the stage's own
        // types after it.
        assert!(
            text.contains("in text · image/* · out text · image/*"),
            "{text}"
        );
        // A stage that takes and hands back types needs a wider box than a
        // plain one, and the box is sized to fit its content whole.
        let plain = content();
        assert!(c.box_width() > plain.box_width(), "wider for its types");
        assert!(!text.contains('…'), "nothing truncated: {text}");
    }

    #[test]
    fn box_width_counts_the_problem_flag_and_the_visit_count() {
        // A long name so the title row drives the width; the problem marker
        // and the visit count then each add to it.
        let mut c = StageNodeContent::from_node(&node(
            "a-stage-with-a-long-name",
            NodeKind::Stage(StageKind::Autonomous),
        ));
        let base = c.box_width();
        c.problem = true;
        c.status = NodeStatus::Visited {
            times: 12,
            errored: false,
        };
        assert!(c.box_width() > base, "the flag and count widen the box");
    }

    #[test]
    fn a_short_area_keeps_its_bounds_and_the_name_gives_way_first() {
        let c = content();
        // One row: brackets, always closed.
        let (_, text) = draw(&c, Rect::new(0, 0, 12, 1), false);
        assert!(
            text.contains(&format!("[ {GLYPH_PENDING} plan ]")),
            "{text}"
        );
        assert!(!text.contains('╭'), "{text}");
        // Too narrow for the name: it is cut with an ellipsis, the closing
        // bracket stays.
        let (_, text) = draw(&c, Rect::new(0, 0, 9, 1), false);
        assert_eq!(
            text.trim_end(),
            format!("[ {GLYPH_PENDING} pl… ]"),
            "{text}"
        );
        // Two rows: a box with the title in its top border and no body.
        let (_, text) = draw(&c, Rect::new(0, 0, 14, 2), false);
        assert!(text.contains(&format!("╭ {GLYPH_PENDING} plan ")), "{text}");
        assert!(text.contains("╰"), "{text}");
        assert!(!text.contains("autonomous"), "{text}");
        // A visit count survives the cut before the name does.
        let mut counted = content();
        counted.status = NodeStatus::Visited {
            times: 12,
            errored: false,
        };
        let (_, text) = draw(&counted, Rect::new(0, 0, 13, 1), false);
        assert_eq!(
            text.trim_end(),
            format!("[ {GLYPH_COMPLETE} pl… ×12 ]"),
            "{text}"
        );
        assert_eq!(fit("plan", 4), "plan");
        assert_eq!(fit("plan", 3), "pl…");
        assert_eq!(fit("plan", 1), "…");
        assert_eq!(fit("plan", 0), "");

        let compact = content();
        let (buf, text) = draw(&compact, Rect::new(0, 0, 14, 1), true);
        assert!(
            text.contains(&format!("[ {GLYPH_PENDING} plan ]")),
            "{text}"
        );
        let bracket = style_at(&buf, "[");
        assert_eq!(bracket.fg, Some(C_BORDER_FOCUS), "selected brackets");
        assert!(bracket.add_modifier.contains(Modifier::BOLD));
        let title = style_at(&buf, "plan");
        assert!(title.add_modifier.contains(Modifier::BOLD));
        assert!(!title.add_modifier.contains(Modifier::REVERSED));
        assert_eq!(NODE_HEIGHT, 4.0);
        // Below the floor clamps to it; above it, the content width is used.
        assert_eq!(node_width(10), MIN_NODE_WIDTH as f64);
        assert_eq!(node_width(40), 40.0);
    }

    #[test]
    fn a_selected_full_node_gets_a_thick_focus_border_and_a_plain_bold_title() {
        let mut c = content();
        c.status = NodeStatus::Current {
            run: RunPhase::Active,
            times: 1,
        };
        let (buf, text) = draw(&c, Rect::new(0, 0, 24, 4), true);
        assert!(text.contains('┏'), "thick frame when selected: {text}");
        assert_eq!(style_at(&buf, "┏").fg, Some(C_BORDER_FOCUS));
        let title = style_at(&buf, "plan");
        assert!(title.add_modifier.contains(Modifier::BOLD));
        assert!(!title.add_modifier.contains(Modifier::REVERSED));
        assert_eq!(
            title.fg,
            Some(C_ACTIVE),
            "the title keeps its status colour"
        );
        let (buf, text) = draw(&c, Rect::new(0, 0, 24, 4), false);
        assert!(text.contains('╭'), "rounded when not: {text}");
        assert_eq!(style_at(&buf, "╭").fg, Some(C_ACTIVE));
        // A 3-row box has room for one detail row and clips the second.
        let (_, text) = draw(&c, Rect::new(0, 0, 24, 3), false);
        assert!(text.contains("autonomous"), "{text}");
    }

    #[test]
    fn edge_style_per_class_back_edge_and_taken() {
        assert_eq!(
            edge_style(EdgeClass::Primary, false, false),
            EdgeStyle::default()
                .with_stroke_style(Style::default().fg(C_MUTED))
                .with_label_style(Style::default().fg(C_MUTED))
        );
        assert_eq!(
            edge_style(EdgeClass::Primary, true, false),
            EdgeStyle::dotted()
                .with_stroke_style(Style::default().fg(C_WARN))
                .with_label_style(Style::default().fg(C_WARN))
        );
        assert_eq!(
            edge_style(EdgeClass::Escape, true, false),
            EdgeStyle::dotted()
                .with_stroke_style(Style::default().fg(C_DIM))
                .with_label_style(Style::default().fg(C_DIM))
        );
        assert_eq!(
            edge_style(EdgeClass::FanOut, false, false),
            EdgeStyle::default()
                .with_line_chars('═', '║')
                .with_corner_chars(['╔', '╗', '╚', '╝'])
                .with_marker_end(EdgeMarker::Diamond)
                .with_stroke_style(Style::default().fg(C_ACCENT))
                .with_label_style(Style::default().fg(C_ACCENT))
        );
        assert_eq!(
            edge_style(EdgeClass::Escape, false, true),
            EdgeStyle::dotted()
                .with_stroke_style(Style::default().fg(C_WHITE))
                .with_label_style(Style::default().fg(C_WHITE))
        );
    }

    #[test]
    fn palette_maps_theme_constants() {
        let p = palette();
        assert_eq!(p.accent, C_ACCENT);
        assert_eq!(p.muted, C_MUTED);
        assert_eq!(p.text, C_WHITE);
        assert_eq!(p.error, C_ERROR);
        assert_eq!(p.canvas_bg, Color::Reset);
    }
}
