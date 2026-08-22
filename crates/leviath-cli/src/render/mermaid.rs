//! Drawing a ```mermaid``` flowchart as an actual diagram.
//!
//! Until now a mermaid block rendered as its own source with a note suggesting
//! you install `mmdc`, which is a diagram nobody can see plus an errand. A
//! terminal cannot run mermaid, but it can draw boxes and arrows, and a
//! flowchart is boxes and arrows.
//!
//! The supported subset is the one agents actually emit: `flowchart`/`graph`
//! with a direction, nodes with the five common shapes, and `-->`, `---`,
//! `-.->` and `==>` edges with optional `|labels|`. Anything else (sequence
//! diagrams, class diagrams, `subgraph` nesting) parses as far as it can and
//! falls back to the source, because a wrong diagram is worse than an honest
//! listing.
//!
//! Layout is layered top to bottom: a node sits one layer below its deepest
//! parent, layers are drawn as a row of boxes, and the edges between
//! consecutive layers are routed through a small character canvas. An edge
//! that goes backwards or skips a layer would need real routing to draw
//! honestly, so it is listed underneath instead of drawn wrong.

use std::collections::HashMap;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::tui::theme::{C_ACCENT, C_BORDER, C_DIM, C_MUTED, C_WHITE};

/// Gap between two boxes on the same layer.
const GAP: usize = 2;
/// Rows a box takes: its two borders and the label between them.
const BOX_ROWS: usize = 3;

/// A node's outline, from its mermaid brackets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `A[text]`, and the default for a bare id.
    Rect,
    /// `A(text)`.
    Round,
    /// `A{text}`, a decision.
    Diamond,
    /// `A((text))`, a terminus.
    Circle,
}

impl Shape {
    /// The pair of characters that top and tail the label inside the box, so a
    /// decision and a terminus read differently without needing more rows.
    fn brackets(self) -> (&'static str, &'static str) {
        match self {
            Self::Rect => (" ", " "),
            Self::Round => ("(", ")"),
            Self::Diamond => ("<", ">"),
            Self::Circle => ("(", ")"),
        }
    }

    /// The corner set the box is drawn with.
    fn corners(self) -> [&'static str; 4] {
        match self {
            Self::Rect => ["┌", "┐", "└", "┘"],
            // Rounded, for everything that is not a plain step.
            _ => ["╭", "╮", "╰", "╯"],
        }
    }
}

#[derive(Debug, Clone)]
struct Node {
    label: String,
    shape: Shape,
}

#[derive(Debug, Clone)]
struct Edge {
    from: usize,
    to: usize,
    label: Option<String>,
    /// A `-.->` edge, drawn with a lighter line.
    dashed: bool,
}

/// A parsed flowchart.
#[derive(Debug, Default)]
struct Chart {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    index: HashMap<String, usize>,
}

impl Chart {
    /// The index of `id`, adding it as a bare node the first time it is seen.
    fn node(&mut self, id: &str, label: Option<String>, shape: Shape) -> usize {
        if let Some(&i) = self.index.get(id) {
            // A later mention that carries a label names a node first seen
            // bare, which is how `A --> B` then `B[Text]` reads.
            if let Some(label) = label {
                self.nodes[i].label = label;
                self.nodes[i].shape = shape;
            }
            return i;
        }
        let i = self.nodes.len();
        self.nodes.push(Node {
            label: label.unwrap_or_else(|| id.to_string()),
            shape,
        });
        self.index.insert(id.to_string(), i);
        i
    }
}

/// Draw `source` as a diagram, or `None` when it is not a flowchart this
/// understands and the caller should show the source instead.
pub(super) fn render(source: &[String], width: u16) -> Option<Vec<Line<'static>>> {
    let chart = parse(source)?;
    let layers = layer(&chart);
    Some(draw(&chart, &layers, width))
}

// ─── Parsing ─────────────────────────────────────────────────────────────────

/// Parse the flowchart subset, or `None` if the block is some other kind of
/// mermaid diagram.
fn parse(source: &[String]) -> Option<Chart> {
    let mut lines = source.iter().map(|l| l.trim()).filter(|l| !l.is_empty());
    let header = lines.next()?;
    let kind = header.split_whitespace().next().unwrap_or_default();
    if !matches!(kind, "flowchart" | "graph") {
        return None;
    }
    let mut chart = Chart::default();
    for line in lines {
        // `subgraph`/`end` are structure this does not draw; their contents
        // are still nodes, so the lines between them still parse.
        if line.starts_with("subgraph") || line == "end" || line.starts_with("%%") {
            continue;
        }
        statement(&mut chart, line);
    }
    (!chart.nodes.is_empty()).then_some(chart)
}

/// One statement: a chain of nodes joined by connectors, or a lone node.
///
/// `A --> B --> C` is two edges, so the walk carries the node it just read
/// forward and closes an edge each time it finds another connector.
fn statement(chart: &mut Chart, line: &str) {
    let Some((token, mut rest)) = next_node(line) else {
        return;
    };
    let mut previous = chart.node(&token.0, token.1, token.2);
    loop {
        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            return;
        }
        let Some((label, dashed, after)) = connector(trimmed) else {
            return;
        };
        let Some((token, next_rest)) = next_node(after) else {
            return;
        };
        let target = chart.node(&token.0, token.1, token.2);
        chart.edges.push(Edge {
            from: previous,
            to: target,
            label,
            dashed,
        });
        previous = target;
        rest = next_rest;
    }
}

/// Read a node at the head of `s`, returning it and whatever follows.
///
/// Everything splits rather than slices by byte: a label holds whatever the
/// document held, and this workspace denies byte-indexing a `str` for exactly
/// the reason that would bite here.
type Token = (String, Option<String>, Shape);
fn next_node(s: &str) -> Option<(Token, &str)> {
    let s = s.trim_start();
    let id: String = s
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if id.is_empty() {
        return None;
    }
    let rest = s.strip_prefix(id.as_str()).unwrap_or_default();
    // A shape opens immediately after the id, with no space between. The two
    // -character openers are tried first so `((round))` is not read as `(`.
    for (open, close, shape) in [
        ("((", "))", Shape::Circle),
        ("[", "]", Shape::Rect),
        ("(", ")", Shape::Round),
        ("{", "}", Shape::Diamond),
    ] {
        if let Some(inner) = rest.strip_prefix(open)
            && let Some((label, after)) = inner.split_once(close)
        {
            return Some(((id, Some(clean(label)), shape), after));
        }
    }
    Some(((id, None, Shape::Rect), rest))
}

/// Strip the quotes mermaid allows around a label.
fn clean(label: &str) -> String {
    let trimmed = label.trim();
    trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(trimmed)
        .to_string()
}

/// Read a connector at the head of `s`: its label, whether it is dashed, and
/// whatever follows it.
fn connector(s: &str) -> Option<(Option<String>, bool, &str)> {
    // Longest first, so `-.->` is not read as `-` and abandoned.
    let arrows = [
        ("-.->", true),
        ("-.-", true),
        ("==>", false),
        ("-->", false),
        ("---", false),
        ("--x", false),
        ("--o", false),
    ];
    let (rest, dashed) = arrows
        .iter()
        .find_map(|(arrow, dashed)| s.strip_prefix(arrow).map(|rest| (rest, *dashed)))?;
    // `A -->|yes| B`
    if let Some(labelled) = rest.strip_prefix('|')
        && let Some((label, after)) = labelled.split_once('|')
    {
        return Some((Some(clean(label)), dashed, after));
    }
    Some((None, dashed, rest))
}

// ─── Layout ──────────────────────────────────────────────────────────────────

/// Nodes grouped into layers, each one below the deepest parent that reaches
/// it along a forward edge.
///
/// Back edges are found first and left out of the sum. Without that, a loop
/// (`A --> B --> A`, which is most interesting flowcharts) drives the depths
/// up until the guard stops them, and the diagram comes out as a column of
/// empty layers. A back edge is listed under the diagram instead.
fn layer(chart: &Chart) -> Vec<Vec<usize>> {
    let back = back_edges(chart);
    let mut depth = vec![0usize; chart.nodes.len()];
    for _ in 0..chart.nodes.len() {
        let mut moved = false;
        for (i, edge) in chart.edges.iter().enumerate() {
            if back[i] || edge.from == edge.to {
                continue;
            }
            if depth[edge.to] < depth[edge.from] + 1 {
                depth[edge.to] = depth[edge.from] + 1;
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
    let deepest = depth.iter().copied().max().unwrap_or(0);
    let mut layers = vec![Vec::new(); deepest + 1];
    for (node, d) in depth.iter().enumerate() {
        layers[*d].push(node);
    }
    // A layer nothing landed on would draw as three blank rows.
    layers.retain(|row: &Vec<usize>| !row.is_empty());
    layers
}

/// Which edges close a loop, by depth-first search: an edge whose target is
/// already on the stack is the one that made the cycle.
fn back_edges(chart: &Chart) -> Vec<bool> {
    let mut back = vec![false; chart.edges.len()];
    let mut state = vec![0u8; chart.nodes.len()]; // 0 unseen, 1 on stack, 2 done
    // (node, how many of its edges have been walked)
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let out_of: Vec<Vec<usize>> = (0..chart.nodes.len())
        .map(|n| {
            chart
                .edges
                .iter()
                .enumerate()
                .filter(|(_, e)| e.from == n)
                .map(|(i, _)| i)
                .collect()
        })
        .collect();
    for root in 0..chart.nodes.len() {
        if state[root] != 0 {
            continue;
        }
        state[root] = 1;
        stack.push((root, 0));
        while let Some((node, walked)) = stack.pop() {
            let Some(&edge) = out_of[node].get(walked) else {
                state[node] = 2;
                continue;
            };
            stack.push((node, walked + 1));
            let target = chart.edges[edge].to;
            match state[target] {
                1 => back[edge] = true,
                0 => {
                    state[target] = 1;
                    stack.push((target, 0));
                }
                _ => {}
            }
        }
    }
    back
}

// ─── Drawing ─────────────────────────────────────────────────────────────────

/// A character grid with a style per cell, blitted into `Line`s at the end.
struct Canvas {
    cells: Vec<Vec<(char, Style)>>,
    width: usize,
}

impl Canvas {
    fn new(width: usize, height: usize) -> Self {
        Self {
            cells: vec![vec![(' ', Style::default()); width]; height],
            width,
        }
    }

    fn put(&mut self, x: usize, y: usize, c: char, style: Style) {
        if x < self.width
            && let Some(row) = self.cells.get_mut(y)
        {
            row[x] = (c, style);
        }
    }

    fn text(&mut self, x: usize, y: usize, s: &str, style: Style) {
        for (i, c) in s.chars().enumerate() {
            self.put(x + i, y, c, style);
        }
    }

    /// Collapse each row into spans, merging runs that share a style so the
    /// buffer does not carry one span per cell.
    fn into_lines(self) -> Vec<Line<'static>> {
        self.cells
            .into_iter()
            .map(|row| {
                let mut spans: Vec<Span<'static>> = Vec::new();
                for (c, style) in row {
                    match spans.last_mut() {
                        Some(last) if last.style == style => last.content.to_mut().push(c),
                        _ => spans.push(Span::styled(c.to_string(), style)),
                    }
                }
                Line::from(spans)
            })
            .collect()
    }
}

/// The width a node's box occupies, always odd.
///
/// Odd so that a box has a true middle column. With even widths two boxes
/// alone on their layers centre one column apart, and the line between them
/// comes out with a kink in it for no reason a reader could name.
fn box_width(node: &Node) -> usize {
    let (open, close) = node.shape.brackets();
    // Two border columns, a space either side of the label, and the shape's
    // own pair of markers.
    let width = node.label.width() + open.width() + close.width() + 4;
    width + (width + 1) % 2
}

/// Where every box sits, and how much room the corridors need.
struct Plan {
    /// Per node: left column, centre column, box width.
    left: Vec<usize>,
    centre: Vec<usize>,
    widths: Vec<usize>,
    /// Per node: the row its box's top border is on.
    top: Vec<usize>,
    /// Rows between one layer's boxes and the next, per gap.
    band: Vec<usize>,
    /// Total rows.
    height: usize,
    /// The column the first side corridor runs down, when there is room for
    /// corridors at all.
    corridor: Option<usize>,
}

/// Edges that do not run from a layer into the one directly below it.
///
/// These are the loops and the shortcuts, and they are what a flowchart is
/// usually *about*. They cannot be drawn straight down without crossing the
/// boxes in between, so each gets its own corridor down the right-hand side.
fn detours(chart: &Chart, depth: &[usize]) -> Vec<usize> {
    (0..chart.edges.len())
        .filter(|i| {
            let edge = &chart.edges[*i];
            depth[edge.to] != depth[edge.from] + 1
        })
        .collect()
}

/// Lay the diagram out: boxes centred per layer, one routing lane per edge
/// that has to move sideways, and a corridor per detour.
fn plan(chart: &Chart, layers: &[Vec<usize>], depth: &[usize], width: usize) -> Plan {
    let widths: Vec<usize> = chart.nodes.iter().map(box_width).collect();
    let detours = detours(chart, depth);
    // Two columns per corridor: the line, and a gap beside it. Dropped
    // entirely when the boxes would be squeezed to nothing by them.
    let wanted = detours.len() * 2 + 1;
    let widest: usize = layers
        .iter()
        .map(|row| {
            row.iter().map(|n| widths[*n]).sum::<usize>() + GAP * row.len().saturating_sub(1)
        })
        .max()
        .unwrap_or(0);
    let corridor = (widest + wanted <= width).then_some(widest + 2);

    let boxes_width = match corridor {
        Some(_) => widest,
        None => width,
    };
    let mut left = vec![0usize; chart.nodes.len()];
    let mut centre = vec![0usize; chart.nodes.len()];
    for row in layers {
        let span: usize =
            row.iter().map(|n| widths[*n]).sum::<usize>() + GAP * row.len().saturating_sub(1);
        let mut x = boxes_width.saturating_sub(span) / 2;
        for node in row {
            left[*node] = x;
            centre[*node] = x + widths[*node] / 2;
            x += widths[*node] + GAP;
        }
    }

    // One lane per edge that moves sideways, so no two share a row and every
    // line can be followed from its box to its arrow head.
    let band: Vec<usize> = (0..layers.len().saturating_sub(1))
        .map(|i| {
            let lanes = chart
                .edges
                .iter()
                .filter(|e| {
                    layers[i].contains(&e.from)
                        && layers[i + 1].contains(&e.to)
                        && centre[e.from] != centre[e.to]
                })
                .count();
            lanes.max(1) + 2
        })
        .collect();

    let mut top = vec![0usize; chart.nodes.len()];
    let mut y = 0usize;
    for (i, row) in layers.iter().enumerate() {
        for node in row {
            top[*node] = y;
        }
        // The last layer has no band under it, so `band` runs out and the
        // total stops at its boxes.
        y += BOX_ROWS + band.get(i).copied().unwrap_or(0);
    }
    let height = y;
    Plan {
        left,
        centre,
        widths,
        top,
        band,
        height,
        corridor,
    }
}

/// Draw the whole diagram: the layered boxes, a lane per edge between them,
/// and a corridor down the side for every edge that loops or skips.
fn draw(chart: &Chart, layers: &[Vec<usize>], width: u16) -> Vec<Line<'static>> {
    let width = (width.max(8) as usize).saturating_sub(1);
    let mut depth = vec![0usize; chart.nodes.len()];
    for (d, row) in layers.iter().enumerate() {
        for node in row {
            depth[*node] = d;
        }
    }
    let plan = plan(chart, layers, &depth, width);
    let mut canvas = Canvas::new(width, plan.height);

    for row in layers {
        for node in row {
            draw_box(
                &mut canvas,
                plan.left[*node],
                plan.top[*node],
                &chart.nodes[*node],
                plan.widths[*node],
            );
        }
    }
    for i in 0..layers.len().saturating_sub(1) {
        route(chart, &mut canvas, &plan, layers, i);
    }
    let listed = corridors(chart, &mut canvas, &plan, &depth);

    let mut out = canvas.into_lines();
    out.extend(listed);
    out
}

/// One node's box.
fn draw_box(canvas: &mut Canvas, x: usize, y: usize, node: &Node, width: usize) {
    let border = Style::default().fg(C_BORDER);
    let [tl, tr, bl, br] = node.shape.corners();
    let label_style = match node.shape {
        Shape::Diamond => Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
        Shape::Circle => Style::default().fg(C_ACCENT),
        _ => Style::default().fg(C_WHITE),
    };
    canvas.text(x, y, tl, border);
    canvas.text(x + width - 1, y, tr, border);
    canvas.text(x, y + 2, bl, border);
    canvas.text(x + width - 1, y + 2, br, border);
    for i in 1..width.saturating_sub(1) {
        canvas.put(x + i, y, '─', border);
        canvas.put(x + i, y + 2, '─', border);
    }
    canvas.put(x, y + 1, '│', border);
    canvas.put(x + width - 1, y + 1, '│', border);
    let (open, close) = node.shape.brackets();
    canvas.text(x + 1, y + 1, &format!(" {open}"), border);
    canvas.text(x + 2 + open.width(), y + 1, &node.label, label_style);
    canvas.text(
        x + 2 + open.width() + node.label.width(),
        y + 1,
        &format!("{close} "),
        border,
    );
}

/// Which way a line leaves a cell, and whether every line through it was
/// drawn with a dashed connector.
#[derive(Default, Clone, Copy)]
struct Meets {
    north: bool,
    south: bool,
    east: bool,
    west: bool,
    /// Set by the first line through; cleared by any solid one.
    dashed: bool,
    seen: bool,
}

impl Meets {
    /// The glyph for what meets here.
    fn glyph(self) -> char {
        // No `┴` or `┬`: a run that continues both ways can only meet a line
        // that continues both ways, because every edge turns on a row of its
        // own. A stem ending on a run's row would need the two to share one,
        // and they never do.
        match (self.north, self.south, self.east, self.west) {
            (true, true, true, true) => '┼',
            (true, true, true, false) => '├',
            (true, true, false, true) => '┤',
            (true, false, true, false) => '╰',
            (true, false, false, true) => '╯',
            (false, true, true, false) => '╭',
            (false, true, false, true) => '╮',
            (_, _, true, _) | (_, _, _, true) => match self.dashed {
                true => '╌',
                false => '─',
            },
            _ => match self.dashed {
                true => '╎',
                false => '│',
            },
        }
    }
}

/// The cells a run of line passes through, and which way it leaves each.
///
/// Collected before anything is drawn rather than painted edge by edge. Two
/// lines can want the same cell - a stem carrying on past a lane another edge
/// turns onto, a box in one layer sitting over a box in the next - and
/// whichever painted last would win, breaking the other. Deciding the glyph
/// from everything that meets there is what keeps both followable.
#[derive(Default)]
struct Lines {
    cells: HashMap<(usize, usize), Meets>,
}

impl Lines {
    fn mark(&mut self, x: usize, y: usize, dashed: bool, f: impl Fn(&mut Meets)) {
        let cell = self.cells.entry((x, y)).or_default();
        f(cell);
        cell.dashed = match cell.seen {
            true => cell.dashed && dashed,
            false => dashed,
        };
        cell.seen = true;
    }

    /// A vertical run down column `x`, from row `top` to row `bottom`.
    fn down(&mut self, x: usize, top: usize, bottom: usize, dashed: bool) {
        for y in top..=bottom {
            self.mark(x, y, dashed, |c| {
                c.north |= y > top;
                c.south |= y < bottom;
            });
        }
    }

    /// A horizontal run along row `y`, between columns `a` and `b`.
    fn across(&mut self, y: usize, a: usize, b: usize, dashed: bool) {
        let (lo, hi) = (a.min(b), a.max(b));
        for x in lo..=hi {
            self.mark(x, y, dashed, |c| {
                c.west |= x > lo;
                c.east |= x < hi;
            });
        }
    }

    fn paint(self, canvas: &mut Canvas, style: Style) {
        for ((x, y), cell) in self.cells {
            canvas.put(x, y, cell.glyph(), style);
        }
    }
}

/// Route layer `i` into layer `i + 1`, one lane per edge.
///
/// A lane of its own is what makes the diagram readable: with every edge
/// sharing one row, two lines that cross merge into a single run and there is
/// no way to tell which end joins which. Here each edge leaves its box, turns
/// onto a row nothing else uses, and turns down again over its target, so a
/// finger can follow it the whole way.
fn route(chart: &Chart, canvas: &mut Canvas, plan: &Plan, layers: &[Vec<usize>], i: usize) {
    let border = Style::default().fg(C_BORDER);
    let arrow = Style::default().fg(C_ACCENT);
    let band_top = plan.top[layers[i][0]] + BOX_ROWS;
    let head = band_top + plan.band[i] - 1;

    let mut lines = Lines::default();
    let mut heads: Vec<(usize, &Edge)> = Vec::new();
    let mut lane = 0usize;
    for source in &layers[i] {
        let outgoing: Vec<&Edge> = chart
            .edges
            .iter()
            .filter(|e| e.from == *source && layers[i + 1].contains(&e.to))
            .collect();
        let from = plan.centre[*source];
        for edge in outgoing {
            let to = plan.centre[edge.to];
            // Marked down to the arrow head's own row, not the one above it:
            // the cell the head lands on is painted over afterwards, and
            // stopping short leaves the last turn with nothing below it.
            if from == to {
                // Straight down: no lane needed, and nothing to cross.
                lines.down(from, band_top, head, edge.dashed);
                heads.push((to, edge));
                continue;
            }
            lane += 1;
            let row = band_top + lane;
            lines.down(from, band_top, row, edge.dashed);
            lines.across(row, from, to, edge.dashed);
            lines.down(to, row, head, edge.dashed);
            heads.push((to, edge));
            // Past the end of the lane rather than on it: a line with a word
            // in the middle of it is a line you cannot follow.
            label(canvas, plan, from.max(to) + 2, row, edge);
        }
    }
    lines.paint(canvas, border);
    for (to, edge) in heads {
        canvas.put(to, head, '▼', arrow);
        if plan.centre[edge.from] == to {
            label(canvas, plan, to + 2, head, edge);
        }
    }
}

/// An edge's label, where it fits without covering the diagram.
fn label(canvas: &mut Canvas, plan: &Plan, x: usize, y: usize, edge: &Edge) {
    let Some(text) = &edge.label else {
        return;
    };
    let room = plan.corridor.unwrap_or(canvas.width);
    if x + text.width() >= room {
        return;
    }
    canvas.text(x, y, text, Style::default().fg(C_MUTED));
}

/// Draw every loop and shortcut down its own corridor beside the diagram,
/// and report the ones there was no room for.
///
/// A corridor is what makes a loop visible at all: the edge leaves its box on
/// the right, runs down a column nothing else uses, and comes back in at its
/// target with a `◀`. Following it is the whole point, so no two share a
/// column.
fn corridors(
    chart: &Chart,
    canvas: &mut Canvas,
    plan: &Plan,
    depth: &[usize],
) -> Vec<Line<'static>> {
    let detours = detours(chart, depth);
    let Some(base) = plan.corridor else {
        return listing(chart, &detours);
    };
    let border = Style::default().fg(C_BORDER);
    for (n, index) in detours.iter().enumerate() {
        let edge = &chart.edges[*index];
        // The corridors were only allocated at all when they all fit, so
        // there is no "this one does not" case to handle.
        let column = base + n * 2;
        let (from, to) = (edge.from, edge.to);
        let (from_row, to_row) = (plan.top[from] + 1, plan.top[to] + 1);
        let from_edge = plan.left[from] + plan.widths[from];
        let to_edge = plan.left[to] + plan.widths[to];
        for x in from_edge..column {
            canvas.put(x, from_row, '─', border);
        }
        for x in to_edge + 1..column {
            canvas.put(x, to_row, '─', border);
        }
        // Corners by the two directions they join. The source end comes from
        // the west and turns away vertically; the target end arrives
        // vertically and turns back west.
        let down = to_row > from_row;
        canvas.put(column, from_row, if down { '╮' } else { '╯' }, border);
        canvas.put(column, to_row, if down { '╯' } else { '╮' }, border);
        let (lo, hi) = (from_row.min(to_row), from_row.max(to_row));
        for y in lo + 1..hi {
            canvas.put(column, y, '│', border);
        }
        canvas.put(to_edge, to_row, '◀', Style::default().fg(C_ACCENT));
        if let Some(text) = &edge.label {
            canvas.text(
                column + 1,
                lo + (hi - lo) / 2,
                text,
                Style::default().fg(C_MUTED),
            );
        }
    }
    Vec::new()
}

/// The detours as a list, for a pane too narrow to run corridors down.
fn listing(chart: &Chart, detours: &[usize]) -> Vec<Line<'static>> {
    detours
        .iter()
        .map(|index| {
            let edge = &chart.edges[*index];
            let label = match &edge.label {
                Some(label) => format!("  ({label})"),
                None => String::new(),
            };
            Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(
                    chart.nodes[edge.from].label.clone(),
                    Style::default().fg(C_WHITE),
                ),
                Span::styled(" ──▶ ", Style::default().fg(C_DIM)),
                Span::styled(
                    chart.nodes[edge.to].label.clone(),
                    Style::default().fg(C_WHITE),
                ),
                Span::styled(label, Style::default().fg(C_MUTED)),
            ])
        })
        .collect()
}
