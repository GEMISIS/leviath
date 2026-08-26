//! The snaking layout's geometry: how wide a row is, how big the boxes and
//! their gaps are, where an edge attaches, and which handles a box needs.
//!
//! A run's path is a chain, and a chain drawn straight runs off the side of
//! any terminal. Snaking it - `per_row` boxes left to right, then the next
//! `per_row` right to left on the row below - keeps it on screen, and keeps
//! the last box of a row directly above the first box of the next, so the
//! hand-off between rows is a short vertical hop instead of a jump back
//! across the canvas. Everything that follows from that shape lives here,
//! away from [`super::view`], which is already the biggest thing in the kit.

use rataflow::{Handle, HandlePosition};

use super::content::{NODE_HEIGHT, node_width};

/// How the boxes are placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayoutMode {
    /// A blueprint: longest-path layers, running the way that fits.
    Layered,
    /// A run's path: a chain snaked across rows `per_row` boxes wide, so it
    /// stays compact and grows downwards while the run is still going.
    Snake { per_row: usize },
}

/// The widest a snake gets before the eye loses the row it was following.
const MAX_PER_ROW: usize = 6;

/// Box size and gaps: `(node_w, node_h, gap_x, gap_y)`.
///
/// A snake is spaced differently from a layered graph: its edges carry no
/// condition label (every hop on a path is `always`), so the columns can sit
/// closer together, while the rows need a gutter the vertical hand-off
/// between them can run down.
pub(crate) fn metrics(longest_id: usize, snake: bool) -> (f64, f64, f64, f64) {
    let (gap_x, gap_y) = if snake { (6.0, 2.0) } else { (8.0, 1.0) };
    (node_width(longest_id), NODE_HEIGHT, gap_x, gap_y)
}

/// How many boxes a snake fits across a canvas `width` cells wide, given the
/// longest node id on it.
///
/// Shared with the detail band, which sizes itself from the number of rows
/// this implies: if the two computed it separately they would disagree the
/// moment one of the constants moved.
pub(crate) fn snake_per_row(longest_id: usize, width: u16) -> usize {
    let (node_w, _, gap_x, _) = metrics(longest_id, true);
    // The same inset the canvas settles inside: the block's border and a cell
    // of margin each side.
    let inner = f64::from(width) - 4.0;
    let fits = ((inner + gap_x) / (node_w + gap_x)).floor();
    // `fits` is finite and small; a negative one (a canvas narrower than its
    // own border) clamps to the single-column snake.
    (fits.max(1.0) as usize).clamp(1, MAX_PER_ROW)
}

/// How many rows of canvas one row of a snake takes: a box, plus the gutter
/// below it the hand-off to the next row runs down.
pub(crate) fn snake_row_pitch() -> u16 {
    let (_, node_h, _, gap_y) = metrics(0, true);
    (node_h + gap_y) as u16
}

/// Which sides an edge leaves and enters on for a snaking path, from the two
/// cells it joins, as `(source, target, loops_back, stem)`.
///
/// Along a row the edge runs the way that row runs, so it leaves the trailing
/// side and enters the leading one; between rows the two boxes share a column
/// (that is what the snake is for), so the hand-off drops straight out of the
/// bottom of one into the top of the next. Both handles are centred, so
/// neither ever needs a lane beside the boxes.
pub(crate) fn route_snake(
    from: (usize, usize),
    to: (usize, usize),
) -> (HandlePosition, HandlePosition, bool, f64) {
    let ((from_row, from_col), (to_row, to_col)) = (from, to);
    let (src, tgt) = if from_row == to_row {
        if to_col > from_col {
            (HandlePosition::Right, HandlePosition::Left)
        } else {
            (HandlePosition::Left, HandlePosition::Right)
        }
    } else {
        (HandlePosition::Bottom, HandlePosition::Top)
    };
    (src, tgt, false, 1.0)
}

/// A snake attaches on all four sides, and to both ends of an edge: a row
/// that runs right to left leaves on the left and enters on the right, which
/// the layered handle set has no pair for. Every one is centred and hidden -
/// a path is never edited, so none of them is a port.
pub(crate) fn handles_snake() -> Vec<Handle> {
    [
        HandlePosition::Left,
        HandlePosition::Right,
        HandlePosition::Top,
        HandlePosition::Bottom,
    ]
    .into_iter()
    .flat_map(|side| {
        [
            Handle::source(side)
                .with_id(side.side_name())
                .with_connectable(false)
                .with_hidden(true),
            Handle::target(side)
                .with_id(side.side_name())
                .with_connectable(false)
                .with_hidden(true),
        ]
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_edges_run_the_way_their_row_runs_and_drop_between_rows() {
        // Along a row: out of the trailing side, into the leading one.
        assert_eq!(
            route_snake((0, 0), (0, 1)),
            (HandlePosition::Right, HandlePosition::Left, false, 1.0)
        );
        // The next row runs the other way, so the edge does too.
        assert_eq!(
            route_snake((1, 3), (1, 2)),
            (HandlePosition::Left, HandlePosition::Right, false, 1.0)
        );
        // Between rows the two boxes share a column: straight down.
        assert_eq!(
            route_snake((0, 3), (1, 3)),
            (HandlePosition::Bottom, HandlePosition::Top, false, 1.0)
        );
        // Never a loop and never a lane, whichever way it went.
        let (_, _, loops_back, stem) = route_snake((1, 0), (2, 0));
        assert!(!loops_back);
        assert_eq!(stem, 1.0);
    }

    #[test]
    fn snake_per_row_fits_what_it_can_between_one_and_the_ceiling() {
        // A 28-cell box and a 6-cell gap: 34 to a column, less the border.
        assert_eq!(snake_per_row(4, 240), MAX_PER_ROW, "capped, not 7");
        assert_eq!(snake_per_row(4, 200), 5);
        assert_eq!(snake_per_row(4, 140), 4);
        assert_eq!(snake_per_row(4, 40), 1);
        // Narrower than its own border still asks for a column, not none.
        assert_eq!(snake_per_row(4, 0), 1);
        // A long stage name makes the boxes wider, so fewer fit.
        assert!(snake_per_row(60, 240) < snake_per_row(4, 240));
        // One row of a snake is a box plus the gutter under it.
        assert_eq!(snake_row_pitch(), 6);
    }

    #[test]
    fn a_snake_offers_every_side_as_both_ends_and_a_layered_graph_is_spaced_wider() {
        let handles = handles_snake();
        assert_eq!(handles.len(), 8, "four sides, each a source and a target");
        assert!(handles.iter().all(|h| h.hidden && !h.connectable));
        // The columns sit closer on a snake and the rows further apart.
        let (snake_w, snake_h, snake_x, snake_y) = metrics(4, true);
        let (layered_w, layered_h, layered_x, layered_y) = metrics(4, false);
        assert_eq!((snake_w, snake_h), (layered_w, layered_h));
        assert!(snake_x < layered_x);
        assert!(snake_y > layered_y);
    }
}
