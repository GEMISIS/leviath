//! Drawing a markdown table into terminal lines.
//!
//! `pulldown-cmark` hands tables over as a stream of cell events. Before this,
//! the renderer ignored every one of them, so a table arrived as its cells run
//! together on one line - the columns, the header and the shape of the thing
//! all gone. Agents emit tables constantly (a comparison, a list of files, a
//! set of options), so that was a common and total loss of meaning.
//!
//! Columns are sized to their content and then squeezed to fit the pane,
//! widest first, because taking a column that needs 4 columns down to 3 costs
//! more than taking one that wanted 30 down to 20.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::tui::theme::{C_BORDER, C_WHITE};

/// How a column's cells sit in their width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Align {
    /// The default, and what a column with no `:---:` marker gets.
    Left,
    Center,
    Right,
}

impl Align {
    /// The alignment `pulldown-cmark` parsed, defaulting to left.
    pub(super) fn of(alignment: &pulldown_cmark::Alignment) -> Self {
        match alignment {
            pulldown_cmark::Alignment::Center => Self::Center,
            pulldown_cmark::Alignment::Right => Self::Right,
            _ => Self::Left,
        }
    }
}

/// One cell: the styled spans its content rendered to.
pub(super) type Cell = Vec<Span<'static>>;

/// A table being accumulated from the event stream.
#[derive(Debug, Default)]
pub(super) struct TableBuilder {
    /// Per column, from the delimiter row.
    pub(super) alignments: Vec<Align>,
    /// The header row. A markdown table cannot be written without one, so
    /// this is a row rather than an option.
    header: Vec<Cell>,
    /// Body rows.
    rows: Vec<Vec<Cell>>,
    /// The row being filled.
    row: Vec<Cell>,
    /// Whether the row being filled is the header.
    in_head: bool,
}

impl TableBuilder {
    pub(super) fn new(alignments: Vec<Align>) -> Self {
        Self {
            alignments,
            ..Self::default()
        }
    }

    pub(super) fn start_head(&mut self) {
        self.in_head = true;
    }

    /// Close the row being filled, into the header or the body.
    pub(super) fn end_row(&mut self) {
        let row = std::mem::take(&mut self.row);
        match self.in_head {
            true => {
                self.header = row;
                self.in_head = false;
            }
            false => self.rows.push(row),
        }
    }

    pub(super) fn push_cell(&mut self, cell: Cell) {
        self.row.push(cell);
    }

    /// Draw the table.
    pub(super) fn draw(self, width: u16) -> Vec<Line<'static>> {
        let mut all: Vec<&Vec<Cell>> = vec![&self.header];
        all.extend(self.rows.iter());
        let columns = all.iter().map(|row| row.len()).max().unwrap_or(0).max(1);
        let widths = fit(&all, columns, width);

        let mut out = Vec::new();
        out.push(rule(&widths, "┌", "┬", "┐"));
        out.push(self.row_line(&self.header, &widths, true));
        out.push(rule(&widths, "├", "┼", "┤"));
        for row in &self.rows {
            out.push(self.row_line(row, &widths, false));
        }
        out.push(rule(&widths, "└", "┴", "┘"));
        out
    }

    /// One row, framed, each cell padded to its column.
    fn row_line(&self, row: &[Cell], widths: &[usize], header: bool) -> Line<'static> {
        let bar = || Span::styled("│", Style::default().fg(C_BORDER));
        let mut spans = vec![bar()];
        for (i, width) in widths.iter().enumerate() {
            let empty = Vec::new();
            let cell = row.get(i).unwrap_or(&empty);
            let align = self.alignments.get(i).copied().unwrap_or(Align::Left);
            spans.push(Span::raw(" "));
            spans.extend(place(cell, *width, align).into_iter().map(|span| {
                // A header cell reads as a heading; a body cell keeps whatever
                // its own markup gave it, over the prose colour.
                let style = match header {
                    true => span
                        .style
                        .patch(Style::default().add_modifier(Modifier::BOLD)),
                    false => Style::default().fg(C_WHITE).patch(span.style),
                };
                Span::styled(span.content, style)
            }));
            spans.push(Span::raw(" "));
            spans.push(bar());
        }
        Line::from(spans)
    }
}

/// A horizontal rule with the given corners and junction.
fn rule(widths: &[usize], left: &str, mid: &str, right: &str) -> Line<'static> {
    let style = Style::default().fg(C_BORDER);
    let mut spans = vec![Span::styled(left.to_string(), style)];
    for (i, width) in widths.iter().enumerate() {
        spans.push(Span::styled("─".repeat(width + 2), style));
        let joint = match i + 1 == widths.len() {
            true => right,
            false => mid,
        };
        spans.push(Span::styled(joint.to_string(), style));
    }
    Line::from(spans)
}

/// Column widths that fit `width`, squeezing the widest column first.
///
/// Every column keeps at least one cell of content, so a table on a very
/// narrow pane is cramped rather than gone.
fn fit(rows: &[&Vec<Cell>], columns: usize, width: u16) -> Vec<usize> {
    let mut widths: Vec<usize> = (0..columns)
        .map(|i| {
            rows.iter()
                .filter_map(|row| row.get(i))
                .map(cell_width)
                .max()
                .unwrap_or(0)
                .max(1)
        })
        .collect();
    // Each column costs its content plus a space either side, and there is one
    // more vertical rule than there are columns.
    let frame = columns * 3 + 1;
    let budget = (width as usize).saturating_sub(frame);
    while widths.iter().sum::<usize>() > budget.max(columns) {
        // `unwrap_or` rather than a guard: there is always at least one
        // column, so "no widest" is a branch no test could take.
        let widest = widths
            .iter()
            .enumerate()
            .max_by_key(|(i, w)| (**w, columns - i))
            .map(|(i, _)| i)
            .unwrap_or(0);
        // No floor test: every column starts at 1, so once the widest is 1 they
        // all are, the sum equals the column count, and the loop has already
        // stopped. A guard here would be a branch nothing can take.
        widths[widest] -= 1;
    }
    widths
}

/// The display width of a cell's spans.
fn cell_width(cell: &Cell) -> usize {
    cell.iter().map(|span| span.content.width()).sum()
}

/// A cell's spans, truncated and padded into `width` under `align`.
fn place(cell: &Cell, width: usize, align: Align) -> Vec<Span<'static>> {
    let mut spans = truncate(cell, width);
    let used = cell_width(&spans);
    let slack = width.saturating_sub(used);
    let (before, after) = match align {
        Align::Left => (0, slack),
        Align::Right => (slack, 0),
        Align::Center => (slack / 2, slack - slack / 2),
    };
    let mut out = Vec::new();
    if before > 0 {
        out.push(Span::raw(" ".repeat(before)));
    }
    out.append(&mut spans);
    if after > 0 {
        out.push(Span::raw(" ".repeat(after)));
    }
    out
}

/// The leading `width` columns of a cell, with `…` where it was cut.
///
/// Char-wise, and measured as it goes: a cell holds whatever the document
/// held, and cutting it by byte would be the panic this workspace forbids.
fn truncate(cell: &Cell, width: usize) -> Vec<Span<'static>> {
    let total = cell_width(cell);
    let mut out = Vec::new();
    if total <= width {
        out.extend(cell.iter().cloned());
        return out;
    }
    // One column goes to the ellipsis that says something was dropped.
    let room = width.saturating_sub(1);
    let mut used = 0usize;
    for span in cell {
        if used >= room {
            break;
        }
        let mut text = String::new();
        for c in span.content.chars() {
            let w = c.to_string().width();
            if used + w > room {
                break;
            }
            used += w;
            text.push(c);
        }
        // Pushed even when empty: a span that contributed nothing renders as
        // nothing, and skipping it would be a branch only a cell cut mid-glyph
        // could reach.
        out.push(Span::styled(text, span.style));
    }
    out.push(Span::styled("…", Style::default().fg(C_BORDER)));
    out
}
