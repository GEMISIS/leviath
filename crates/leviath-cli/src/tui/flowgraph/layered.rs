//! The layered layout's geometry: which sides an edge leaves and enters on,
//! how far it travels before turning, and which handles a box needs.
//!
//! The counterpart of [`super::snake`] for a blueprint drawn on layers, kept
//! out of [`super::view`] for the same reason that one is.

use rataflow::{Handle, HandlePosition};

use super::layout::Direction;
use super::model::EdgeClass;

/// Which sides an edge leaves and enters on, and how far it travels before
/// turning, for a graph flowing in `direction`.
///
/// The main flow runs along the layer axis. Edges that go against it (a
/// loop back to an earlier layer, or out of a stage that only escapes reach)
/// leave and re-enter on the same side and run along a lane beside the
/// nodes: below them (right of them, top-to-bottom) for the flow's own
/// loops, above (left) for the escapes. A lane per layer of distance keeps a
/// long loop from overwriting a short one's label.
pub(super) fn route(
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

/// Handles on every side, named by side so an edge can pick where it
/// attaches. The forward sides (out along the flow, in against it) sit at
/// the middle of their edge; the lane sides carry the source a little past
/// the middle and the target a little before it, so a loop leaves and
/// re-enters at different cells. Hidden on a viewer, where handle glyphs
/// would read as ports; on an editor the flow-side pair shows and connects,
/// and the lanes stay hidden (routing, not ports).
pub(super) fn handles(direction: Direction, edit: bool) -> Vec<Handle> {
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
