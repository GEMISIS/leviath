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
/// Rows of routing between one layer of boxes and the next.
const ROUTE_ROWS: usize = 3;

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
    let (layers, back) = layer(&chart);
    Some(draw(&chart, &layers, &back, width))
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
fn layer(chart: &Chart) -> (Vec<Vec<usize>>, Vec<bool>) {
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
    (layers, back)
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

/// Draw the whole diagram: a title line, the layered boxes, and a listing of
/// any edge that could not be drawn between adjacent layers.
fn draw(chart: &Chart, layers: &[Vec<usize>], back: &[bool], width: u16) -> Vec<Line<'static>> {
    let width = width.max(8) as usize;
    let widths: Vec<usize> = chart.nodes.iter().map(box_width).collect();
    // Where each layer's boxes start, and each box's centre column.
    let mut centre = vec![0usize; chart.nodes.len()];
    let mut left = vec![0usize; chart.nodes.len()];
    for row in layers {
        let span: usize =
            row.iter().map(|n| widths[*n]).sum::<usize>() + GAP * row.len().saturating_sub(1);
        let mut x = (width.saturating_sub(span)) / 2;
        for node in row {
            left[*node] = x;
            centre[*node] = x + widths[*node] / 2;
            x += widths[*node] + GAP;
        }
    }

    let height = layers.len() * 3 + layers.len().saturating_sub(1) * ROUTE_ROWS;
    let mut canvas = Canvas::new(width, height);
    let border = Style::default().fg(C_BORDER);

    for (i, row) in layers.iter().enumerate() {
        let top = i * (3 + ROUTE_ROWS);
        for node in row {
            draw_box(
                &mut canvas,
                left[*node],
                top,
                &chart.nodes[*node],
                widths[*node],
            );
        }
        if i + 1 < layers.len() {
            route(
                chart,
                &canvas_rows(top),
                &mut canvas,
                &centre,
                layers,
                i,
                border,
            );
        }
    }

    let mut out = canvas.into_lines();
    out.extend(extras(chart, layers, back));
    out
}

/// The three routing rows under a layer whose boxes start at `top`.
fn canvas_rows(top: usize) -> [usize; 3] {
    [top + 3, top + 4, top + 5]
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

/// Route the edges from layer `i` into layer `i + 1`.
///
/// Grouped by source, because the junction a line turns through depends on
/// every edge leaving that node at once: one going right is a `╰`, one each way
/// is a `┴`. Drawing them one at a time would put an elbow where a tee belongs.
fn route(
    chart: &Chart,
    rows: &[usize; 3],
    canvas: &mut Canvas,
    centre: &[usize],
    layers: &[Vec<usize>],
    i: usize,
    border: Style,
) {
    let here = &layers[i];
    let next = &layers[i + 1];
    // Columns where a line leaves a box downwards. A target that arrives at
    // one of them is a crossing, not an elbow, and the target pass would
    // otherwise paint over the junction the source pass just drew.
    let mut junctions: Vec<usize> = Vec::new();
    for source in here {
        let outgoing: Vec<&Edge> = chart
            .edges
            .iter()
            .filter(|e| e.from == *source && next.contains(&e.to))
            .collect();
        if outgoing.is_empty() {
            continue;
        }
        let from = centre[*source];
        let dashed = outgoing.iter().all(|e| e.dashed);
        canvas.put(from, rows[0], if dashed { '╎' } else { '│' }, border);

        for edge in &outgoing {
            let to = centre[edge.to];
            let (lo, hi) = (from.min(to), from.max(to));
            for x in lo..=hi {
                canvas.put(x, rows[1], if dashed { '╌' } else { '─' }, border);
            }
        }
        // The junction under the source, once every direction is known.
        let left = outgoing.iter().any(|e| centre[e.to] < from);
        let right = outgoing.iter().any(|e| centre[e.to] > from);
        let down = outgoing.iter().any(|e| centre[e.to] == from);
        canvas.put(
            from,
            rows[1],
            match (left, right, down) {
                (true, true, _) => '┴',
                (true, false, true) => '┤',
                (false, true, true) => '├',
                (true, false, false) => '╯',
                (false, true, false) => '╰',
                (false, false, _) => '│',
            },
            border,
        );
        junctions.push(from);
    }

    // Then the arrow heads, once every run is down. A target's mark cannot be
    // read off the canvas: the next source's run paints straight over it, so
    // it is decided from the edges that arrive there instead.
    let mut by_target: Vec<(usize, Vec<&Edge>)> = Vec::new();
    for edge in &chart.edges {
        if !here.contains(&edge.from) || !next.contains(&edge.to) {
            continue;
        }
        match by_target.iter_mut().find(|(t, _)| *t == edge.to) {
            Some((_, arriving)) => arriving.push(edge),
            None => by_target.push((edge.to, vec![edge])),
        }
    }
    for (target, incoming) in by_target {
        let to = centre[target];
        let left = incoming.iter().any(|e| centre[e.from] < to);
        let right = incoming.iter().any(|e| centre[e.from] > to);
        // A column that is already a source junction has a line running down
        // through it as well as sideways.
        let crossing = junctions.contains(&to);
        if let Some(mark) = match (left, right, crossing) {
            (false, false, _) => None,
            (_, _, true) => Some('┼'),
            // Reached from both sides, so the run passes through it.
            (true, true, false) => Some('┬'),
            (true, false, false) => Some('╮'),
            (false, true, false) => Some('╭'),
        } {
            canvas.put(to, rows[1], mark, border);
        }
        canvas.put(to, rows[2], '▼', Style::default().fg(C_ACCENT));
        if let Some(label) = incoming.iter().find_map(|e| e.label.as_ref()) {
            canvas.text(to + 2, rows[2], label, Style::default().fg(C_MUTED));
        }
    }
}

/// Edges that do not run between adjacent layers, listed rather than drawn.
///
/// A back edge or a layer-skipping edge needs routing around the boxes in
/// between; naming it is honest, and drawing it through them would not be.
fn extras(chart: &Chart, layers: &[Vec<usize>], back: &[bool]) -> Vec<Line<'static>> {
    let mut depth = vec![0usize; chart.nodes.len()];
    for (d, row) in layers.iter().enumerate() {
        for node in row {
            depth[*node] = d;
        }
    }
    let mut out = Vec::new();
    for (i, edge) in chart.edges.iter().enumerate() {
        if !back[i] && depth[edge.to] == depth[edge.from] + 1 {
            continue;
        }
        let label = match &edge.label {
            Some(label) => format!("  ({label})"),
            None => String::new(),
        };
        out.push(Line::from(vec![
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
        ]));
    }
    out
}
