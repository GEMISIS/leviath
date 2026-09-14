//! One cursor for every vertical list the TUI draws: where it moves to, and
//! which rows are on screen around it.
//!
//! A click that resolves against a different window of rows than the one
//! drawn is exactly the drift two copies of the offset arithmetic produce, so
//! a widget's hit-test and its draw both ask here.

/// `cursor` moved by `delta` within a list of `len` rows, clamped to the ends
/// rather than wrapped: at eighty models a wrap from the top to the bottom
/// reads as the list jumping rather than moving. An empty list holds the
/// cursor at the top.
pub(crate) fn move_cursor(cursor: usize, delta: isize, len: usize) -> usize {
    let last = len.saturating_sub(1) as isize;
    (cursor as isize + delta).clamp(0, last) as usize
}

/// The first row drawn so that `cursor` is on screen in a list `height` rows
/// tall: the top of the list until the cursor reaches the bottom row, then a
/// window that slides with it.
pub(crate) fn window_start(cursor: usize, height: usize) -> usize {
    cursor.saturating_sub(height.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_clamp_to_the_list() {
        assert_eq!(move_cursor(0, -1, 5), 0);
        assert_eq!(move_cursor(4, 1, 5), 4);
        assert_eq!(move_cursor(2, 1, 5), 3);
        assert_eq!(move_cursor(2, -1, 5), 1);
        assert_eq!(move_cursor(3, 1, 0), 0);
    }

    #[test]
    fn the_window_follows_the_cursor_off_the_bottom() {
        assert_eq!(window_start(0, 4), 0);
        assert_eq!(window_start(3, 4), 0);
        assert_eq!(window_start(4, 4), 1);
        assert_eq!(window_start(9, 0), 9);
    }
}
