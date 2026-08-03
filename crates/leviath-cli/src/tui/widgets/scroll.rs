//! Tail-anchored scroll state for append-only panes (logs, output).
//!
//! The offset is measured from the tail: `0` means pinned to the newest
//! content (the pane follows appends), anything greater holds a position
//! relative to the end. Anchoring to the tail rather than the head means a
//! bounded buffer dropping its oldest entries never yanks the view around.

/// Scroll position for a pane whose content appends at the bottom.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScrollState {
    /// Rows between the bottom of the view and the newest entry. `0` = tailing.
    pub(crate) offset_from_tail: usize,
}

impl ScrollState {
    /// Whether the pane is pinned to the newest content.
    pub(crate) fn is_tailing(&self) -> bool {
        self.offset_from_tail == 0
    }

    /// Scroll back through history by `n` rows, clamped so the view never
    /// scrolls past the oldest entry.
    pub(crate) fn scroll_up(&mut self, n: usize, len: usize, viewport: usize) {
        let max = len.saturating_sub(viewport);
        self.offset_from_tail = (self.offset_from_tail + n).min(max);
    }

    /// Scroll toward the newest content; landing on it resumes tailing.
    pub(crate) fn scroll_down(&mut self, n: usize) {
        self.offset_from_tail = self.offset_from_tail.saturating_sub(n);
    }

    /// Jump to the oldest entry.
    pub(crate) fn jump_to_top(&mut self, len: usize, viewport: usize) {
        self.offset_from_tail = len.saturating_sub(viewport);
    }

    /// Jump back to the newest entry and resume tailing.
    pub(crate) fn jump_to_tail(&mut self) {
        self.offset_from_tail = 0;
    }

    /// The range of entries to draw in a `viewport`-row window.
    pub(crate) fn window(&mut self, len: usize, viewport: usize) -> std::ops::Range<usize> {
        // Re-clamp against the current length: the content may have shrunk
        // since the last scroll (a cleared buffer).
        self.offset_from_tail = self.offset_from_tail.min(len.saturating_sub(viewport));
        let end = len - self.offset_from_tail.min(len);
        end.saturating_sub(viewport)..end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_state_tails_and_windows_the_newest_rows() {
        let mut s = ScrollState::default();
        assert!(s.is_tailing());
        assert_eq!(s.window(10, 4), 6..10);
        // Content shorter than the viewport: the whole thing.
        assert_eq!(s.window(2, 4), 0..2);
        assert_eq!(s.window(0, 4), 0..0);
    }

    #[test]
    fn scrolling_up_holds_a_position_and_clamps_at_the_oldest_entry() {
        let mut s = ScrollState::default();
        s.scroll_up(3, 10, 4);
        assert_eq!(s.window(10, 4), 3..7);
        assert!(!s.is_tailing());

        s.scroll_up(100, 10, 4);
        assert_eq!(s.window(10, 4), 0..4, "clamped at the top");
    }

    #[test]
    fn scrolling_down_returns_to_the_tail() {
        let mut s = ScrollState::default();
        s.scroll_up(5, 10, 4);
        s.scroll_down(2);
        assert_eq!(s.window(10, 4), 3..7);
        s.scroll_down(100);
        assert!(s.is_tailing());
        assert_eq!(s.window(10, 4), 6..10);
    }

    #[test]
    fn top_and_tail_jumps() {
        let mut s = ScrollState::default();
        s.jump_to_top(10, 4);
        assert_eq!(s.window(10, 4), 0..4);
        s.jump_to_tail();
        assert_eq!(s.window(10, 4), 6..10);
        // A tiny list makes to_top a no-op rather than an underflow.
        let mut short = ScrollState::default();
        short.jump_to_top(2, 4);
        assert!(short.is_tailing());
    }

    #[test]
    fn a_shrinking_buffer_reclamps_the_held_position() {
        let mut s = ScrollState::default();
        s.scroll_up(6, 10, 4);
        assert_eq!(s.offset_from_tail, 6);
        // The buffer shrank underneath the held position.
        assert_eq!(s.window(5, 4), 0..4);
        assert_eq!(s.offset_from_tail, 1);
    }
}
