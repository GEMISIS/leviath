//! Mouse text selection for the dashboard's text panes.
//!
//! Selection is anchored to *screen cells*, not to the pane's source lines.
//! Panes wrap long lines at draw time, so the only representation that always
//! matches what the user sees is the drawn frame itself: the highlight is
//! painted over the frame buffer after the panes render, and the copied text
//! is read back from those same cells. This is the semantics of native
//! terminal selection, which mouse capture otherwise takes away.
//!
//! Lifecycle: a left-button press anywhere starts a drag (the whole frame is
//! one selection region, exactly like native terminal selection - a
//! multi-row drag spans panes and borders alike), dragging extends the
//! highlight, and release copies the highlighted cells through the same
//! clipboard seam as `y` yank. A plain click, any scroll, or a resize (which
//! moves text under a screen-anchored highlight) clears the selection.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use unicode_width::UnicodeWidthStr;

use super::state::Dashboard;
use super::types::ToastLevel;

/// An in-progress or just-released mouse selection over one pane.
///
/// Coordinates are absolute screen cells `(column, row)`, both endpoints
/// inclusive and always inside `region`.
pub(super) struct Selection {
    /// The selectable region the drag started in - the whole frame, as
    /// registered by `draw()` each frame.
    pub(super) region: Rect,
    /// Where the drag started.
    pub(super) anchor: (u16, u16),
    /// Where the drag currently is (or ended).
    pub(super) cursor: (u16, u16),
    /// True between button press and release.
    pub(super) dragging: bool,
    /// True once any drag motion arrived; release without motion is a plain
    /// click and clears instead of copying.
    pub(super) moved: bool,
    /// Set on release; the next draw extracts the text and copies it.
    pub(super) pending_copy: bool,
}

/// The registered pane containing the given screen cell, if any.
fn hit_region(regions: &[Rect], column: u16, row: u16) -> Option<Rect> {
    regions
        .iter()
        .copied()
        .find(|r| r.contains(Position::new(column, row)))
}

/// Clamp a screen cell into a non-empty rect (a drag may wander outside the
/// pane it started in).
fn clamp_to_region(region: Rect, column: u16, row: u16) -> (u16, u16) {
    (
        column.clamp(region.left(), region.right().saturating_sub(1)),
        row.clamp(region.top(), region.bottom().saturating_sub(1)),
    )
}

/// Order two endpoints into reading order (top-to-bottom, then left-to-right),
/// so a drag upward or backward selects the same range as its mirror.
fn ordered(a: (u16, u16), b: (u16, u16)) -> ((u16, u16), (u16, u16)) {
    if (a.1, a.0) <= (b.1, b.0) {
        (a, b)
    } else {
        (b, a)
    }
}

/// The inclusive column range a selection covers on one row: partial on the
/// first and last rows, the full pane width in between.
fn row_span(region: Rect, start: (u16, u16), end: (u16, u16), row: u16) -> (u16, u16) {
    let first = if row == start.1 {
        start.0
    } else {
        region.left()
    };
    let last = if row == end.1 {
        end.0
    } else {
        region.right().saturating_sub(1)
    };
    (first, last)
}

/// Read the selected cells back out of the drawn frame as plain text.
///
/// Rows are trimmed of the buffer's padding spaces and joined with newlines,
/// which is what native terminal selection produces. Wide symbols (CJK,
/// emoji) occupy continuation cells in the buffer; advancing by each symbol's
/// display width skips them so nothing is doubled.
fn extract_from_buffer(buf: &Buffer, region: Rect, a: (u16, u16), b: (u16, u16)) -> String {
    let (start, end) = ordered(a, b);
    let mut rows = Vec::new();
    for row in start.1..=end.1 {
        let (first, last) = row_span(region, start, end, row);
        let mut text = String::new();
        let mut column = first;
        while column <= last {
            let Some(cell) = buf.cell(Position::new(column, row)) else {
                break;
            };
            let symbol = cell.symbol();
            text.push_str(symbol);
            column += (UnicodeWidthStr::width(symbol).max(1)) as u16;
        }
        rows.push(text.trim_end().to_string());
    }
    rows.join("\n")
}

/// Paint the selection over the drawn frame by inverting each selected cell.
/// `REVERSED` composes with whatever style the pane drew (including search
/// highlights) instead of erasing it.
fn highlight_in_buffer(buf: &mut Buffer, region: Rect, a: (u16, u16), b: (u16, u16)) {
    let (start, end) = ordered(a, b);
    for row in start.1..=end.1 {
        let (first, last) = row_span(region, start, end, row);
        for column in first..=last {
            if let Some(cell) = buf.cell_mut(Position::new(column, row)) {
                let style = cell.style().add_modifier(Modifier::REVERSED);
                cell.set_style(style);
            }
        }
    }
}

impl Dashboard {
    /// Single entry point for every mouse event, so the wheel and selection
    /// cannot disagree about state. The wheel scrolls the pane under the
    /// cursor, hit-tested against the rects each renderer registered this
    /// frame - not whichever pane the keyboard last touched.
    pub(super) fn handle_mouse(&mut self, event: MouseEvent) {
        // A formatting button on a long-form editor takes a press before any
        // pane does: the toolbar is drawn over the pane behind it, so a press
        // routed by position alone would act on that pane instead.
        if matches!(event.kind, MouseEventKind::Down(MouseButton::Left))
            && self.markdown_toolbar_click(event.column, event.row)
        {
            return;
        }
        // The Agents screen's chooser and canvas take the mouse first.
        if self.agent_builder.is_some() && self.handle_agents_mouse(event) {
            return;
        }
        // A graph canvas takes the mouse before the selection machinery: a
        // pan is not a copy highlight.
        if self.route_mouse_to_graph(event) {
            return;
        }
        match event.kind {
            MouseEventKind::ScrollUp => self.wheel_scroll(event.column, event.row, 3),
            MouseEventKind::ScrollDown => self.wheel_scroll(event.column, event.row, -3),
            MouseEventKind::Down(MouseButton::Left) => {
                self.selection_begin(event.column, event.row);
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.selection_drag(event.column, event.row);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                // A release with no motion behind it is a click, not a
                // selection: it goes to whatever was drawn under it (see
                // `click.rs`). A drag stays a copy highlight.
                let plain_click = self.selection_release(event.column, event.row);
                if plain_click {
                    self.handle_click(event.column, event.row);
                }
            }
            _ => {}
        }
    }

    /// Route a wheel notch to the pane under the cursor. `lines > 0` scrolls
    /// back through history (up), matching the keyboard convention.
    fn wheel_scroll(&mut self, column: u16, row: u16, lines: i32) {
        let hit = self
            .pane_rects
            .iter()
            .find(|(_, rect)| {
                column >= rect.x
                    && column < rect.x + rect.width
                    && row >= rect.y
                    && row < rect.y + rect.height
            })
            .map(|(id, _)| *id);
        match hit {
            Some(super::types::PaneId::LogPanel) => {
                let (len, viewport) = (self.log.len(), self.log_viewport.max(1));
                if lines >= 0 {
                    self.log_scroll
                        .scroll_up(lines.unsigned_abs() as usize, len, viewport);
                } else {
                    self.log_scroll.scroll_down(lines.unsigned_abs() as usize);
                }
                self.selection = None;
            }
            Some(super::types::PaneId::RunTable) => {
                // The wheel moves the selection through the run list.
                let delta = if lines >= 0 { -1isize } else { 1isize };
                if !self.display_indices.is_empty() {
                    let max = self.display_indices.len() - 1;
                    self.selected = self.selected.saturating_add_signed(delta).min(max);
                    self.table_state.select(Some(self.selected));
                }
            }
            // Detail content, or anywhere unregistered: the keyboard's target.
            _ => self.scroll_by(lines),
        }
    }

    /// A left press inside a selectable pane starts a drag there; anywhere
    /// else it clears whatever selection existed.
    fn selection_begin(&mut self, column: u16, row: u16) {
        self.selection = hit_region(&self.selection_regions, column, row).map(|region| Selection {
            region,
            anchor: (column, row),
            cursor: (column, row),
            dragging: true,
            moved: false,
            pending_copy: false,
        });
    }

    fn selection_drag(&mut self, column: u16, row: u16) {
        if let Some(sel) = self.selection.as_mut().filter(|s| s.dragging) {
            sel.cursor = clamp_to_region(sel.region, column, row);
            sel.moved = true;
        }
    }

    /// Release ends the drag: with motion it schedules the copy for the next
    /// draw (extraction needs the drawn buffer), without motion it was a plain
    /// click and clears.
    ///
    /// Returns whether this was a plain click, so the caller can hand it to
    /// the click targets. A release with no drag behind it at all (the press
    /// landed outside every selectable region) counts as one too - the frame
    /// is one selection region, so in practice this is a release the graph
    /// canvases let through.
    fn selection_release(&mut self, column: u16, row: u16) -> bool {
        let Some(sel) = self.selection.as_mut().filter(|s| s.dragging) else {
            return true;
        };
        sel.cursor = clamp_to_region(sel.region, column, row);
        sel.dragging = false;
        if sel.moved {
            sel.pending_copy = true;
            false
        } else {
            self.selection = None;
            true
        }
    }

    /// Drop a selection whose pane was not drawn this frame. Panes re-register
    /// their rects every draw, so one containment check covers resize, view
    /// switches, the document editor taking over the content pane, and any
    /// other layout change that moved the text out from under the highlight.
    pub(super) fn validate_selection(&mut self) {
        if let Some(sel) = &self.selection
            && !self.selection_regions.contains(&sel.region)
        {
            self.selection = None;
        }
    }

    /// Post-render pass: paint the highlight over the drawn panes and, on the
    /// draw after release, read the highlighted text back and copy it.
    ///
    /// Runs after the panes render (so the cells exist) and before toasts and
    /// overlays (so those draw over the highlight, and the copy's toast shows
    /// this same frame).
    pub(super) fn apply_selection_overlay(&mut self, frame: &mut Frame) {
        self.validate_selection();
        let Some(sel) = &self.selection else {
            return;
        };
        let (region, anchor, cursor, pending) =
            (sel.region, sel.anchor, sel.cursor, sel.pending_copy);
        highlight_in_buffer(frame.buffer_mut(), region, anchor, cursor);
        if pending {
            let text = extract_from_buffer(frame.buffer_mut(), region, anchor, cursor);
            self.selection = None;
            self.finish_copy(&text);
        }
    }

    /// Copy released-selection text through the same clipboard seam as `y`
    /// yank. Whitespace-only selections (an empty stretch of pane) copy
    /// nothing and show nothing.
    fn finish_copy(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        if (self.yank_fn)(text) {
            let chars = text.chars().count();
            let lines = text.lines().count();
            let message = if lines > 1 {
                format!("Copied {} chars ({} lines)", chars, lines)
            } else {
                format!("Copied {} chars", chars)
            };
            self.toast(message, ToastLevel::Info);
        } else {
            self.toast(
                "Clipboard unavailable (no pbcopy/xclip/OSC52)",
                ToastLevel::Error,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::make_test_dashboard;
    use super::super::types::{AgentDisplayStatus, DashboardAgent, LogEntry, PaneId};

    fn wheel(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    fn plain_agent(id: &str) -> DashboardAgent {
        DashboardAgent {
            id: id.to_string(),
            blueprint_name: "test-agent".to_string(),
            stage: "main".to_string(),
            stage_index: 0,
            num_stages: 1,
            status: AgentDisplayStatus::Active,
            tokens_in: 0,
            tokens_out: 0,
            cached_tokens: 0,
            iteration: 0,
            waiting_prompt: None,
            wait_reason: None,
            pending_request: None,
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
            workdir: "/tmp".to_string(),
            task: "task".to_string(),
            title: None,
            model: None,
            parent_id: None,
            depth: 0,
            started_at: 1000,
            last_progress_at: None,
            active_until: None,
            waiting_secs: 0,
            graph: None,
            accepts_messages: true,
            taint_summary: vec![],
        }
    }

    #[test]
    fn the_wheel_scrolls_the_pane_under_the_cursor() {
        let mut dash = make_test_dashboard();
        dash.log.clear();
        for i in 0..50 {
            dash.log.push(LogEntry {
                timestamp: "t".to_string(),
                message: format!("line {i}"),
            });
        }
        dash.log_viewport = 10;
        let mut second = plain_agent("run-2");
        second.started_at = 900;
        dash.agents.push(plain_agent("run-1"));
        dash.agents.push(second);
        dash.update_display_indices();
        dash.pane_rects = vec![
            (PaneId::RunTable, Rect::new(0, 0, 80, 20)),
            (PaneId::LogPanel, Rect::new(0, 20, 80, 15)),
        ];

        // Over the log: wheel up scrolls history, wheel down returns.
        dash.handle_mouse(wheel(MouseEventKind::ScrollUp, 5, 25));
        assert_eq!(dash.log_scroll.offset_from_tail, 3);
        dash.handle_mouse(wheel(MouseEventKind::ScrollDown, 5, 25));
        assert!(dash.log_scroll.is_tailing());

        // Over the run table: the wheel moves the selection.
        dash.handle_mouse(wheel(MouseEventKind::ScrollDown, 5, 5));
        assert_eq!(dash.selected, 1);
        dash.handle_mouse(wheel(MouseEventKind::ScrollUp, 5, 5));
        assert_eq!(dash.selected, 0);
        // …and clamps at both ends.
        dash.handle_mouse(wheel(MouseEventKind::ScrollUp, 5, 5));
        assert_eq!(dash.selected, 0);

        // Outside both rects: falls back to the keyboard target.
        dash.detail_view = true;
        dash.handle_mouse(wheel(MouseEventKind::ScrollUp, 100, 38));
        assert_eq!(dash.detail_scroll, 3);

        // Over the table with nothing listed: a safe no-op.
        dash.display_indices.clear();
        dash.handle_mouse(wheel(MouseEventKind::ScrollDown, 5, 5));
        assert_eq!(dash.selected, 0);
    }

    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::widgets::Paragraph;

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn region() -> Rect {
        Rect::new(2, 1, 10, 4)
    }

    // ── Pure geometry ────────────────────────────────────────────────────

    #[test]
    fn hit_region_finds_the_containing_rect() {
        // Panes never overlap in the real layout, so the fixture keeps the
        // rects disjoint.
        let other = Rect::new(20, 10, 5, 5);
        let regions = [other, region()];
        assert_eq!(hit_region(&regions, 3, 2), Some(region()));
        assert_eq!(hit_region(&regions, 22, 12), Some(other));
    }

    #[test]
    fn hit_region_misses_outside_and_on_the_exclusive_edge() {
        let regions = [region()];
        // right() and bottom() are exclusive: the last cell inside is (11, 4).
        assert_eq!(hit_region(&regions, 11, 4), Some(region()));
        assert_eq!(hit_region(&regions, 12, 4), None);
        assert_eq!(hit_region(&regions, 11, 5), None);
        assert_eq!(hit_region(&[], 3, 2), None);
    }

    #[test]
    fn clamp_to_region_pulls_outside_points_to_the_edges() {
        let r = region();
        assert_eq!(clamp_to_region(r, 0, 0), (2, 1));
        assert_eq!(clamp_to_region(r, 50, 50), (11, 4));
        assert_eq!(clamp_to_region(r, 5, 2), (5, 2));
    }

    #[test]
    fn ordered_sorts_by_row_then_column() {
        assert_eq!(ordered((3, 1), (7, 2)), ((3, 1), (7, 2)));
        assert_eq!(ordered((7, 2), (3, 1)), ((3, 1), (7, 2)));
        // Same row, dragged right-to-left.
        assert_eq!(ordered((9, 2), (4, 2)), ((4, 2), (9, 2)));
    }

    #[test]
    fn row_span_is_partial_on_first_and_last_rows_and_full_between() {
        let r = region();
        let start = (5, 1);
        let end = (8, 3);
        assert_eq!(row_span(r, start, end, 1), (5, 11));
        assert_eq!(row_span(r, start, end, 2), (2, 11));
        assert_eq!(row_span(r, start, end, 3), (2, 8));
    }

    #[test]
    fn row_span_on_a_single_row_selection_uses_both_endpoints() {
        let r = region();
        assert_eq!(row_span(r, (4, 2), (9, 2), 2), (4, 9));
    }

    // ── Buffer extraction + highlight ────────────────────────────────────

    #[test]
    fn extract_single_row_takes_the_exact_column_range() {
        let buf = Buffer::with_lines(["hello world"]);
        let r = Rect::new(0, 0, 11, 1);
        assert_eq!(extract_from_buffer(&buf, r, (6, 0), (10, 0)), "world");
    }

    #[test]
    fn extract_multi_row_spans_full_middle_rows_and_trims_padding() {
        let buf = Buffer::with_lines(["first line   ", "middle       ", "last line    "]);
        let r = Rect::new(0, 0, 13, 3);
        let text = extract_from_buffer(&buf, r, (6, 0), (3, 2));
        assert_eq!(text, "line\nmiddle\nlast");
    }

    #[test]
    fn extract_preserves_interior_empty_rows() {
        let buf = Buffer::with_lines(["top", "   ", "bottom"]);
        let r = Rect::new(0, 0, 6, 3);
        assert_eq!(
            extract_from_buffer(&buf, r, (0, 0), (5, 2)),
            "top\n\nbottom"
        );
    }

    #[test]
    fn extract_skips_wide_char_continuation_cells() {
        let buf = Buffer::with_lines(["日本語 ok"]);
        let r = Rect::new(0, 0, 9, 1);
        assert_eq!(extract_from_buffer(&buf, r, (0, 0), (8, 0)), "日本語 ok");
    }

    #[test]
    fn extract_reversed_drag_matches_forward_drag() {
        let buf = Buffer::with_lines(["hello world"]);
        let r = Rect::new(0, 0, 11, 1);
        assert_eq!(
            extract_from_buffer(&buf, r, (10, 0), (6, 0)),
            extract_from_buffer(&buf, r, (6, 0), (10, 0)),
        );
    }

    #[test]
    fn extract_stops_at_the_buffer_edge_when_the_region_overhangs() {
        // A region wider than the buffer must not panic; extraction takes
        // whatever cells exist.
        let buf = Buffer::with_lines(["abc"]);
        let overhanging = Rect::new(0, 0, 10, 1);
        assert_eq!(
            extract_from_buffer(&buf, overhanging, (0, 0), (9, 0)),
            "abc"
        );
    }

    #[test]
    fn highlight_ignores_cells_beyond_the_buffer_edge() {
        let mut buf = Buffer::with_lines(["abc"]);
        let overhanging = Rect::new(0, 0, 10, 1);
        highlight_in_buffer(&mut buf, overhanging, (0, 0), (9, 0));
        let reversed = |x: u16| {
            buf.cell(Position::new(x, 0))
                .unwrap()
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        };
        assert!(reversed(0) && reversed(2));
    }

    #[test]
    fn highlight_reverses_selected_cells_and_leaves_the_rest() {
        let mut buf = Buffer::with_lines(["hello world"]);
        let r = Rect::new(0, 0, 11, 1);
        highlight_in_buffer(&mut buf, r, (6, 0), (10, 0));
        let reversed = |x: u16| {
            buf.cell(Position::new(x, 0))
                .unwrap()
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        };
        assert!(reversed(6) && reversed(10));
        assert!(!reversed(0) && !reversed(5));
    }

    // ── Mouse event handling ─────────────────────────────────────────────

    #[test]
    fn wheel_events_still_scroll_three_lines_per_notch() {
        let mut dash = make_test_dashboard();
        dash.handle_mouse(mouse(MouseEventKind::ScrollUp, 0, 0));
        assert_eq!(dash.detail_scroll, 3);
        dash.handle_mouse(mouse(MouseEventKind::ScrollDown, 0, 0));
        assert_eq!(dash.detail_scroll, 0);
    }

    #[test]
    fn left_down_inside_a_registered_pane_starts_a_drag() {
        let mut dash = make_test_dashboard();
        dash.selection_regions.push(region());
        dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 4, 2));
        let sel = dash.selection.as_ref().unwrap();
        assert_eq!(sel.anchor, (4, 2));
        assert_eq!(sel.cursor, (4, 2));
        assert!(sel.dragging && !sel.moved && !sel.pending_copy);
    }

    #[test]
    fn left_down_outside_any_pane_clears_the_selection() {
        let mut dash = make_test_dashboard();
        dash.selection_regions.push(region());
        dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 4, 2));
        assert!(dash.selection.is_some());
        dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 50, 50));
        assert!(dash.selection.is_none());
    }

    #[test]
    fn drag_moves_the_cursor_and_clamps_to_the_pane() {
        let mut dash = make_test_dashboard();
        dash.selection_regions.push(region());
        dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 4, 2));
        dash.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 50, 50));
        let sel = dash.selection.as_ref().unwrap();
        assert_eq!(sel.cursor, (11, 4));
        assert!(sel.moved);
    }

    #[test]
    fn drag_without_an_active_selection_is_ignored() {
        let mut dash = make_test_dashboard();
        dash.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 4, 2));
        assert!(dash.selection.is_none());
    }

    #[test]
    fn release_after_motion_schedules_the_copy() {
        let mut dash = make_test_dashboard();
        dash.selection_regions.push(region());
        dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 4, 2));
        dash.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 8, 2));
        dash.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 8, 2));
        let sel = dash.selection.as_ref().unwrap();
        assert!(!sel.dragging && sel.pending_copy);
    }

    #[test]
    fn release_without_motion_is_a_click_and_clears() {
        let mut dash = make_test_dashboard();
        dash.selection_regions.push(region());
        dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 4, 2));
        dash.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 4, 2));
        assert!(dash.selection.is_none());
    }

    #[test]
    fn release_without_a_prior_press_is_ignored() {
        let mut dash = make_test_dashboard();
        dash.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 4, 2));
        assert!(dash.selection.is_none());
    }

    #[test]
    fn drag_after_release_no_longer_extends_the_selection() {
        let mut dash = make_test_dashboard();
        dash.selection_regions.push(region());
        dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 4, 2));
        dash.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 8, 2));
        dash.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 8, 2));
        dash.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 10, 3));
        assert_eq!(dash.selection.as_ref().unwrap().cursor, (8, 2));
    }

    #[test]
    fn other_buttons_and_motion_events_are_ignored() {
        let mut dash = make_test_dashboard();
        dash.selection_regions.push(region());
        dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Right), 4, 2));
        dash.handle_mouse(mouse(MouseEventKind::Moved, 4, 2));
        assert!(dash.selection.is_none());
        assert_eq!(dash.detail_scroll, 0);
    }

    #[test]
    fn scrolling_clears_an_active_selection() {
        let mut dash = make_test_dashboard();
        dash.selection_regions.push(region());
        dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 4, 2));
        assert!(dash.selection.is_some());
        dash.scroll_by(1);
        assert!(dash.selection.is_none());
    }

    // ── Validation + overlay ─────────────────────────────────────────────

    #[test]
    fn validate_drops_a_selection_whose_pane_was_not_redrawn() {
        let mut dash = make_test_dashboard();
        dash.selection_regions.push(region());
        dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 4, 2));
        dash.selection_regions.clear();
        dash.validate_selection();
        assert!(dash.selection.is_none());
    }

    #[test]
    fn validate_keeps_a_selection_whose_pane_is_still_registered() {
        let mut dash = make_test_dashboard();
        dash.selection_regions.push(region());
        dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 4, 2));
        dash.validate_selection();
        assert!(dash.selection.is_some());
    }

    /// Draw `text` at the origin, run the selection overlay, and return the
    /// backend buffer for assertions.
    fn draw_with_overlay(dash: &mut Dashboard, text: &str, area: Rect) -> Buffer {
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                f.render_widget(Paragraph::new(text.to_string()), area);
                dash.selection_regions.push(area);
                dash.apply_selection_overlay(f);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn overlay_highlights_the_dragged_range() {
        let mut dash = make_test_dashboard();
        let area = Rect::new(0, 0, 20, 2);
        dash.selection_regions.push(area);
        dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 0));
        dash.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 6, 0));
        dash.selection_regions.clear();
        let buf = draw_with_overlay(&mut dash, "hello world", area);
        let reversed = |x: u16| {
            buf.cell(Position::new(x, 0))
                .unwrap()
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        };
        assert!(reversed(2) && reversed(6));
        assert!(!reversed(0) && !reversed(7));
        // Still an active drag: nothing copied, selection retained.
        assert!(dash.selection.is_some());
        assert!(dash.toasts.is_empty());
    }

    #[test]
    fn overlay_without_a_selection_does_nothing() {
        let mut dash = make_test_dashboard();
        let area = Rect::new(0, 0, 20, 2);
        let buf = draw_with_overlay(&mut dash, "hello world", area);
        let any_reversed = (0..20).any(|x| {
            buf.cell(Position::new(x, 0))
                .unwrap()
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        });
        assert!(!any_reversed);
    }

    /// A clipboard recorder for `fn(&str) -> bool` seams: captures the copied
    /// text into a static slot. Each test gets its own module (and so its own
    /// slot), because tests run in parallel and a shared slot would race.
    macro_rules! clipboard_recorder {
        ($name:ident) => {
            mod $name {
                use std::sync::Mutex;
                pub(super) static COPIED: Mutex<String> = Mutex::new(String::new());
                pub(super) fn capture(text: &str) -> bool {
                    *COPIED.lock().unwrap() = text.to_string();
                    true
                }
            }
        };
    }
    clipboard_recorder!(single_line_recorder);
    clipboard_recorder!(multi_line_recorder);

    /// A clipboard that always refuses. Shared by the failure-toast test
    /// (which executes it) and the whitespace test (which must not reach it;
    /// the absence of any toast proves that, since a call in either direction
    /// would have pushed one).
    fn deny(_: &str) -> bool {
        false
    }

    fn select_range(dash: &mut Dashboard, area: Rect, from: (u16, u16), to: (u16, u16)) {
        dash.selection_regions.push(area);
        dash.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            from.0,
            from.1,
        ));
        dash.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), to.0, to.1));
        dash.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), to.0, to.1));
        dash.selection_regions.clear();
    }

    #[test]
    fn released_selection_copies_the_visible_text_and_toasts() {
        let mut dash = make_test_dashboard();
        dash.yank_fn = single_line_recorder::capture;
        let area = Rect::new(0, 0, 20, 2);
        select_range(&mut dash, area, (6, 0), (10, 0));
        draw_with_overlay(&mut dash, "hello world", area);
        assert_eq!(*single_line_recorder::COPIED.lock().unwrap(), "world");
        assert!(dash.selection.is_none());
        assert_eq!(dash.toasts.len(), 1);
        assert_eq!(dash.toasts[0].message, "Copied 5 chars");
    }

    #[test]
    fn multi_line_copy_reports_the_line_count() {
        let mut dash = make_test_dashboard();
        dash.yank_fn = multi_line_recorder::capture;
        let area = Rect::new(0, 0, 10, 3);
        select_range(&mut dash, area, (0, 0), (2, 1));
        draw_with_overlay(&mut dash, "abc\ndef", area);
        assert_eq!(*multi_line_recorder::COPIED.lock().unwrap(), "abc\ndef");
        assert_eq!(dash.toasts[0].message, "Copied 7 chars (2 lines)");
    }

    #[test]
    fn whitespace_only_selection_copies_nothing_and_stays_silent() {
        let mut dash = make_test_dashboard();
        dash.yank_fn = deny;
        let area = Rect::new(0, 0, 20, 2);
        select_range(&mut dash, area, (0, 1), (5, 1));
        draw_with_overlay(&mut dash, "text on row zero only", area);
        assert!(dash.selection.is_none());
        assert!(dash.toasts.is_empty());
    }

    #[test]
    fn clipboard_failure_shows_the_unavailable_toast() {
        let mut dash = make_test_dashboard();
        dash.yank_fn = deny;
        let area = Rect::new(0, 0, 20, 2);
        select_range(&mut dash, area, (0, 0), (4, 0));
        draw_with_overlay(&mut dash, "hello world", area);
        assert_eq!(
            dash.toasts[0].message,
            "Clipboard unavailable (no pbcopy/xclip/OSC52)"
        );
    }
}
