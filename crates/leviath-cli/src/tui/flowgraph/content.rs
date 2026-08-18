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
    Idle,
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
            RunPhase::Idle | RunPhase::Cancelled => C_DIM,
        }
    }

    fn glyph(self, tick: u64) -> &'static str {
        match self {
            RunPhase::Active => SPINNER[(tick as usize) % SPINNER.len()],
            RunPhase::Waiting => GLYPH_WAITING,
            RunPhase::Paused | RunPhase::Idle => GLYPH_PENDING,
            RunPhase::Stale | RunPhase::Error => GLYPH_ERROR,
            RunPhase::Complete => GLYPH_COMPLETE,
            RunPhase::Cancelled => "⊘",
        }
    }

    fn word(self) -> &'static str {
        match self {
            RunPhase::Active => "running",
            RunPhase::Waiting => "waiting",
            RunPhase::Paused => "paused",
            RunPhase::Idle => "idle",
            RunPhase::Stale => "stale",
            RunPhase::Complete => "complete",
            RunPhase::Error => "error",
            RunPhase::Cancelled => "cancelled",
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

/// How much of the box to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeStyle {
    /// A bordered box with a title row and two detail rows.
    Full,
    /// One row: `[ glyph name ×n ]`.
    Compact,
}

impl NodeStyle {
    /// Node height on the canvas.
    pub(crate) fn height(self) -> f64 {
        match self {
            NodeStyle::Full => 4.0,
            NodeStyle::Compact => 1.0,
        }
    }

    /// Node width for the longest id on the graph.
    pub(crate) fn width(self, longest_id: usize) -> f64 {
        match self {
            // `╭ ▶ ⠋ name ×12 ╮`, and room for `⑂ 3 run · 1 done · 0 fail`.
            NodeStyle::Full => (longest_id + 10).max(28) as f64,
            // `[ ⠋ name ×12 ]`
            NodeStyle::Compact => (longest_id + 9).max(14) as f64,
        }
    }
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
    pub(crate) max_iterations: Option<usize>,
    pub(crate) style: NodeStyle,
    // ── live ──
    pub(crate) status: NodeStatus,
    /// Iterations taken in the current visit, when this is the current stage.
    pub(crate) iteration: Option<usize>,
    /// When the stage was last entered, as `HH:MM:SS`.
    pub(crate) last_seen: Option<String>,
    /// Workers of a fan-out stage that is running now.
    pub(crate) workers: Option<WorkerCounts>,
    pub(crate) tick: u64,
}

impl StageNodeContent {
    /// The blueprint's view of a node, before any run touched it.
    pub(crate) fn from_node(node: &StageNode, style: NodeStyle) -> Self {
        Self {
            name: node.id.trim_start_matches("ext:").to_string(),
            kind_label: node.kind_label(),
            is_external: node.kind == NodeKind::ExternalBlueprint,
            is_entry: node.is_entry,
            is_terminal: node.is_terminal || node.allow_complete,
            self_loop: node.self_loop,
            max_iterations: node.max_iterations,
            style,
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

    fn title(&self, selected: bool) -> Line<'static> {
        let (glyph, colour) = self.status.look(self.tick);
        let mut style = Style::default().fg(colour);
        if matches!(self.status, NodeStatus::Current { .. }) {
            style = style.add_modifier(Modifier::BOLD);
        }
        if selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let mut text = String::new();
        if self.is_entry {
            text.push_str("▶ ");
        }
        text.push_str(glyph);
        text.push(' ');
        text.push_str(&self.name);
        let times = self.status.times();
        if times > 1 {
            text.push_str(&format!(" ×{times}"));
        }
        Line::from(Span::styled(format!(" {text} "), style))
    }

    /// The first detail row: what the stage is, and how far the current
    /// visit has got.
    fn detail_row(&self) -> String {
        if let Some(w) = self.workers {
            return format!("⑂ {} run · {} done · {} fail", w.running, w.done, w.failed);
        }
        let mut parts: Vec<String> = vec![self.kind_label.to_string()];
        if let Some(iteration) = self.iteration {
            match self.max_iterations {
                Some(max) => parts.push(format!("iter {iteration}/{max}")),
                None => parts.push(format!("iter {iteration}")),
            }
        }
        if let NodeStatus::Current { run, .. } = self.status
            && run != RunPhase::Active
        {
            parts.push(run.word().to_string());
        }
        parts.join(" · ")
    }

    /// The second detail row: badges.
    fn badge_row(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.self_loop {
            parts.push("↺ loops".to_string());
        }
        if self.is_terminal {
            parts.push("⏹ can end".to_string());
        }
        if let Some(seen) = &self.last_seen {
            parts.push(seen.clone());
        }
        parts.join(" · ")
    }
}

impl NodeContent for StageNodeContent {
    fn render(&self, ctx: &NodeRenderContext, buf: &mut Buffer) {
        let area = ctx.area;
        let (_, colour) = self.status.look(self.tick);
        let border_colour = if ctx.selected { C_BORDER_FOCUS } else { colour };
        // Zoomed out, or compact: the box would swallow the label, so draw
        // just the title.
        if self.style == NodeStyle::Compact || area.height < 3 || area.width < 6 {
            let mut line = self.title(ctx.selected);
            if area.width >= 6 {
                line.spans
                    .insert(0, Span::styled("[", Style::default().fg(border_colour)));
                line.spans
                    .push(Span::styled("]", Style::default().fg(border_colour)));
            }
            Paragraph::new(line).render(area, buf);
            return;
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(if self.is_external {
                BorderType::Double
            } else {
                BorderType::Rounded
            })
            .border_style(Style::default().fg(border_colour))
            .title(self.title(ctx.selected));
        let inner = block.inner(area);
        block.render(area, buf);
        let rows = [
            Line::from(Span::styled(
                format!(" {}", self.detail_row()),
                Style::default().fg(C_MUTED),
            )),
            Line::from(Span::styled(
                format!(" {}", self.badge_row()),
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

    fn content(style: NodeStyle) -> StageNodeContent {
        StageNodeContent::from_node(&node("plan", NodeKind::Stage(StageKind::Autonomous)), style)
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
        let mut c = content(NodeStyle::Full);
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
                "autonomous · iter 3/9",
            ),
            (RunPhase::Waiting, GLYPH_WAITING, C_WARN, "waiting"),
            (RunPhase::Paused, GLYPH_PENDING, C_WARN, "paused"),
            (RunPhase::Idle, GLYPH_PENDING, C_DIM, "idle"),
            (RunPhase::Stale, GLYPH_ERROR, C_WARN, "stale"),
            (RunPhase::Complete, GLYPH_COMPLETE, C_SUCCESS, "complete"),
            (RunPhase::Error, GLYPH_ERROR, C_ERROR, "error"),
            (RunPhase::Cancelled, "⊘", C_DIM, "cancelled"),
        ];
        c.tick = 3;
        c.iteration = Some(3);
        c.max_iterations = Some(9);
        for (run, glyph, colour, word) in phases {
            c.status = NodeStatus::Current { run, times: 1 };
            let (buf, text) = draw(&c, area, false);
            assert!(text.contains(&format!("{glyph} plan")), "{run:?}: {text}");
            assert!(text.contains(word), "{run:?}: {text}");
            let style = style_at(&buf, "plan");
            assert_eq!(style.fg, Some(colour), "{run:?}");
            assert!(style.add_modifier.contains(Modifier::BOLD), "{run:?}");
        }
        // Without a ceiling the iteration stands alone.
        c.max_iterations = None;
        c.status = NodeStatus::Current {
            run: RunPhase::Active,
            times: 3,
        };
        let (_, text) = draw(&c, area, false);
        assert!(text.contains("iter 3"), "{text}");
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
        let mut c = StageNodeContent::from_node(&n, NodeStyle::Full);
        assert_eq!(c.kind_label, "fan-out");
        c.last_seen = Some("14:22:01".to_string());
        c.workers = Some(WorkerCounts {
            running: 3,
            done: 2,
            failed: 1,
        });
        let (_, text) = draw(&c, Rect::new(0, 0, 40, 4), false);
        assert!(text.contains("▶"), "entry marker: {text}");
        assert!(text.contains("⑂ 3 run · 2 done · 1 fail"), "{text}");
        assert!(text.contains("↺ loops · ⏹ can end · 14:22:01"), "{text}");
        c.clear_live();
        assert_eq!(c.status, NodeStatus::Pending);
        assert!(c.workers.is_none() && c.last_seen.is_none() && c.iteration.is_none());

        let ext = StageNodeContent::from_node(
            &node("ext:researcher", NodeKind::ExternalBlueprint),
            NodeStyle::Full,
        );
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
    fn a_tiny_area_degrades_to_the_bare_label_and_compact_is_one_row() {
        let c = content(NodeStyle::Full);
        let (_, text) = draw(&c, Rect::new(0, 0, 12, 1), false);
        assert!(
            text.contains(&format!("[ {GLYPH_PENDING} plan ]")),
            "{text}"
        );
        assert!(!text.contains('╭'), "{text}");
        // Narrower than the brackets: just the words.
        let (_, text) = draw(&c, Rect::new(0, 0, 5, 1), false);
        assert!(text.starts_with(&format!(" {GLYPH_PENDING} pl")), "{text}");
        assert!(!text.contains('['), "{text}");

        let compact = content(NodeStyle::Compact);
        let (buf, text) = draw(&compact, Rect::new(0, 0, 14, 1), true);
        assert!(
            text.contains(&format!("[ {GLYPH_PENDING} plan ]")),
            "{text}"
        );
        assert!(
            style_at(&buf, "[").fg == Some(C_BORDER_FOCUS),
            "selected brackets"
        );
        assert!(
            style_at(&buf, "plan")
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(NodeStyle::Compact.height(), 1.0);
        assert_eq!(NodeStyle::Full.height(), 4.0);
        assert_eq!(NodeStyle::Compact.width(3), 14.0);
        assert_eq!(NodeStyle::Compact.width(10), 19.0);
        assert_eq!(NodeStyle::Full.width(10), 28.0);
        assert_eq!(NodeStyle::Full.width(20), 30.0);
    }

    #[test]
    fn a_selected_full_node_gets_the_focus_border_and_reversed_title() {
        let mut c = content(NodeStyle::Full);
        c.status = NodeStatus::Current {
            run: RunPhase::Active,
            times: 1,
        };
        let (buf, _) = draw(&c, Rect::new(0, 0, 24, 4), true);
        assert_eq!(style_at(&buf, "╭").fg, Some(C_BORDER_FOCUS));
        assert!(
            style_at(&buf, "plan")
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        let (buf, _) = draw(&c, Rect::new(0, 0, 24, 4), false);
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
