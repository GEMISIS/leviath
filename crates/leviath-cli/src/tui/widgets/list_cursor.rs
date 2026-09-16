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

/// [`window_start`] for rows that are not all one line tall (a note that
/// wrapped): the top of the list until the cursor's row would run off the
/// bottom, then a window whose last whole row is the cursor's. With every
/// height 1 the two agree.
pub(crate) fn window_start_by_height(heights: &[usize], cursor: usize, height: usize) -> usize {
    let Some(last) = heights.len().checked_sub(1) else {
        return 0;
    };
    let cursor = cursor.min(last);
    let mut start = cursor;
    let mut used = heights[cursor];
    while start > 0 && used + heights[start - 1] <= height {
        start -= 1;
        used += heights[start];
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tall_rows_shrink_the_window_and_unit_rows_match_the_plain_one() {
        let unit = [1; 10];
        for cursor in 0..10 {
            assert_eq!(
                window_start_by_height(&unit, cursor, 4),
                window_start(cursor, 4)
            );
        }
        // Rows of 1, 5, 1, 1 in 4 lines: the cursor on the last row shows
        // the two short rows above it and not the tall one.
        assert_eq!(window_start_by_height(&[1, 5, 1, 1], 3, 4), 2);
        // A row taller than the window still starts the window.
        assert_eq!(window_start_by_height(&[1, 5, 1, 1], 1, 4), 1);
        assert_eq!(window_start_by_height(&[], 3, 4), 0);
        assert_eq!(
            window_start_by_height(&[1, 1], 9, 4),
            0,
            "a cursor past the end"
        );
    }

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
