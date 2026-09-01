//! A full-screen modal for putting a short list in the order you want: drag a
//! row by its grip, or move the one under the cursor with the arrows.
//!
//! The setup wizard opens this for the provider priority, which is an ordering
//! rather than a single choice, so the [`Picker`](super::picker::Picker) - built
//! to filter and pick one of many - is the wrong shape. The drag mirrors the
//! agent editor's model-chain reorder: a `⠿` grip lifts a row and drops it where
//! the pointer is, and the list is not mutated until the drop.

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::popup::{centered, popup_frame};
use crate::tui::theme::{C_ACCENT, C_ACTIVE, C_DIM, C_MUTED, C_WHITE};

/// The grip that lifts a row, and the width of the gutter reserved for it.
const GRIP: &str = "⠿ ";
const GRIP_W: u16 = 2;

/// What a key or click did to the modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReorderOutcome {
    /// Still ordering.
    Pending,
    /// Kept, with the list in the order it now holds.
    Confirmed(Vec<String>),
    /// Closed without keeping the change.
    Cancelled,
}

/// One row: the value that will be written, and a few words on what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReorderItem {
    /// The value written back (a provider name).
    pub(crate) value: String,
    /// A short note shown beside it (e.g. "configured", "not configured").
    pub(crate) detail: String,
}

/// A drag in progress: the row it was lifted from, and where the pointer is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Drag {
    from: usize,
    to: usize,
}

/// The modal's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reorder {
    /// Its heading.
    title: String,
    /// A line or two on what the order decides.
    explain: Vec<String>,
    /// The rows, in their current order.
    items: Vec<ReorderItem>,
    /// Which row the cursor is on.
    cursor: usize,
    /// A drag in progress, if any.
    drag: Option<Drag>,
}

impl Reorder {
    /// A modal open on the first row.
    pub(crate) fn new(
        title: impl Into<String>,
        explain: Vec<String>,
        items: Vec<ReorderItem>,
    ) -> Self {
        Self {
            title: title.into(),
            explain,
            items,
            cursor: 0,
            drag: None,
        }
    }

    /// The order to draw, and to keep: the live list, except that while a row
    /// is being dragged it shows lifted out of `from` and dropped at `to`. The
    /// list itself is not touched until the drop, so what is under the pointer
    /// is always the answer.
    fn ordered(&self) -> Vec<ReorderItem> {
        match self.drag {
            Some(drag) => self.ordered_from(drag),
            None => self.items.clone(),
        }
    }

    /// The values in their current order, for a caller keeping the result.
    fn values(&self) -> Vec<String> {
        self.ordered().into_iter().map(|i| i.value).collect()
    }

    /// The rows as `(value, detail)`, for a caller (a test) checking what the
    /// modal was seeded with.
    #[cfg(test)]
    pub(crate) fn rows_for_test(&self) -> Vec<(String, String)> {
        self.items
            .iter()
            .map(|i| (i.value.clone(), i.detail.clone()))
            .collect()
    }

    /// Move the cursor, clamped to the list.
    fn move_cursor(&mut self, delta: isize) {
        let next = self.cursor as isize + delta;
        self.cursor = next.clamp(0, self.items.len().saturating_sub(1) as isize) as usize;
    }

    /// Move the row under the cursor one place, the cursor following it. A move
    /// off either end is a no-op rather than a wrap: a list this short reads a
    /// wrap as a jump.
    fn move_row(&mut self, delta: isize) {
        let to = self.cursor as isize + delta;
        if to < 0 || to >= self.items.len() as isize {
            return;
        }
        let to = to as usize;
        let held = self.items.remove(self.cursor);
        self.items.insert(to, held);
        self.cursor = to;
    }

    /// Keys while the modal is open.
    pub(crate) fn handle_key(&mut self, key: &KeyEvent) -> ReorderOutcome {
        use crossterm::event::KeyModifiers as Mods;
        let shifted = key.modifiers.contains(Mods::SHIFT);
        match key.code {
            KeyCode::Up if shifted => self.move_row(-1),
            KeyCode::Down if shifted => self.move_row(1),
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Enter => return ReorderOutcome::Confirmed(self.values()),
            KeyCode::Esc => return ReorderOutcome::Cancelled,
            _ => {}
        }
        ReorderOutcome::Pending
    }

    /// The mouse while the modal is open: the wheel moves the cursor, a press
    /// on a row's grip lifts it, a drag moves it, and the release drops it.
    pub(crate) fn handle_mouse(&mut self, mouse: &MouseEvent, area: Rect) -> ReorderOutcome {
        match mouse.kind {
            MouseEventKind::ScrollDown => self.move_cursor(1),
            MouseEventKind::ScrollUp => self.move_cursor(-1),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(row) = self.row_at(area, mouse.row) {
                    self.cursor = row;
                    // Only the grip starts a drag; a click on the rest of the
                    // row just moves the cursor, so a value stays readable.
                    if self.on_grip(area, mouse.column) {
                        self.drag = Some(Drag { from: row, to: row });
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Moved => {
                let over = self.row_at(area, mouse.row);
                if let (Some(drag), Some(row)) = (self.drag.as_mut(), over) {
                    drag.to = row;
                    self.cursor = row;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(drag) = self.drag.take() {
                    self.items = self.ordered_from(drag);
                    self.cursor = drag.to.min(self.items.len().saturating_sub(1));
                }
            }
            _ => {}
        }
        ReorderOutcome::Pending
    }

    /// [`ordered`](Self::ordered) for a specific drag, used at the drop.
    fn ordered_from(&self, drag: Drag) -> Vec<ReorderItem> {
        let mut items = self.items.clone();
        if drag.from < items.len() && drag.to < items.len() {
            let held = items.remove(drag.from);
            items.insert(drag.to, held);
        }
        items
    }

    /// The list's split: the prose, then the rows.
    fn layout(&self, inner: Rect) -> std::rc::Rc<[Rect]> {
        let explain = (self.explain.len() as u16 + 2).min(inner.height.saturating_sub(2));
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(explain), Constraint::Min(1)])
            .split(inner)
    }

    /// The inner rect the popup leaves, or `None` when the window is too short
    /// to draw anything clickable. Shared by drawing and hit-testing so a
    /// click cannot resolve against a row that is not on screen.
    fn list_rect(&self, area: Rect) -> Option<Rect> {
        let popup = centered(70, 70, area);
        let inner = Block::default().borders(Borders::ALL).inner(popup);
        if inner.height < 3 {
            return None;
        }
        Some(self.layout(inner)[1])
    }

    /// Which row a click landed on. The list is short enough to never scroll,
    /// so the row is the offset from the list's top.
    pub(crate) fn row_at(&self, area: Rect, row: u16) -> Option<usize> {
        let list = self.list_rect(area)?;
        if row < list.y || row >= list.y + list.height {
            return None;
        }
        let position = (row - list.y) as usize;
        (position < self.items.len()).then_some(position)
    }

    /// Whether a click column is on the grip gutter (as opposed to the rest of
    /// the row). Only a grip press starts a drag.
    fn on_grip(&self, area: Rect, column: u16) -> bool {
        let Some(list) = self.list_rect(area) else {
            return false;
        };
        column >= list.x && column < list.x + GRIP_W
    }

    /// Draw the modal over everything else.
    pub(crate) fn draw(&self, frame: &mut Frame, area: Rect) {
        let popup = centered(70, 70, area);
        let inner = popup_frame(frame, popup, &self.title, C_ACCENT);
        let chunks = self.layout(inner);

        let mut lines: Vec<Line<'static>> = self
            .explain
            .iter()
            .map(|text| Line::from(Span::styled(text.clone(), Style::default().fg(C_MUTED))))
            .collect();
        lines.push(Line::from(Span::styled(
            "Drag ⠿, or Shift+↑/↓ to move a row. Enter keeps the order, Esc cancels.",
            Style::default().fg(C_DIM),
        )));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), chunks[0]);

        // The row the pointer is holding, so it can be marked while dragged.
        let held = self.drag.map(|d| d.to);
        let rows: Vec<Line<'static>> = self
            .ordered()
            .iter()
            .enumerate()
            .map(|(position, item)| {
                let on = position == self.cursor;
                let dragged = Some(position) == held;
                Line::from(vec![
                    Span::styled(GRIP, Style::default().fg(if on { C_ACCENT } else { C_DIM })),
                    Span::styled(format!("{}. ", position + 1), Style::default().fg(C_DIM)),
                    Span::styled(
                        format!("{:<16}", item.value),
                        if on || dragged {
                            Style::default().fg(C_ACTIVE).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(C_WHITE)
                        },
                    ),
                    Span::styled(item.detail.clone(), Style::default().fg(C_DIM)),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(rows), chunks[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn items(values: &[&str]) -> Vec<ReorderItem> {
        values
            .iter()
            .map(|v| ReorderItem {
                value: v.to_string(),
                detail: "configured".to_string(),
            })
            .collect()
    }

    fn reorder() -> Reorder {
        Reorder::new(
            "Provider priority",
            vec!["best first".to_string()],
            items(&["a", "b", "c"]),
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn shift(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn arrows_move_the_cursor_clamped() {
        let mut r = reorder();
        assert_eq!(r.handle_key(&key(KeyCode::Up)), ReorderOutcome::Pending);
        assert_eq!(r.cursor, 0, "clamped at the top");
        r.handle_key(&key(KeyCode::Down));
        r.handle_key(&key(KeyCode::Down));
        r.handle_key(&key(KeyCode::Down));
        assert_eq!(r.cursor, 2, "clamped at the bottom");
    }

    #[test]
    fn shift_arrows_move_the_row_and_the_cursor_follows() {
        let mut r = reorder();
        // Move the top row down two places: a, b, c -> b, c, a.
        r.handle_key(&shift(KeyCode::Down));
        r.handle_key(&shift(KeyCode::Down));
        assert_eq!(r.values(), ["b", "c", "a"]);
        assert_eq!(r.cursor, 2, "the cursor rode with the row");
        // And back up one: b, c, a -> b, a, c.
        r.handle_key(&shift(KeyCode::Up));
        assert_eq!(r.values(), ["b", "a", "c"]);
    }

    #[test]
    fn a_row_move_off_either_end_is_a_no_op() {
        let mut r = reorder();
        r.handle_key(&shift(KeyCode::Up));
        assert_eq!(r.values(), ["a", "b", "c"], "the top row cannot go up");
        r.move_cursor(isize::MAX);
        r.handle_key(&shift(KeyCode::Down));
        assert_eq!(r.values(), ["a", "b", "c"], "the bottom row cannot go down");
    }

    #[test]
    fn enter_confirms_the_current_order_and_esc_cancels() {
        let mut r = reorder();
        r.handle_key(&shift(KeyCode::Down));
        assert_eq!(
            r.handle_key(&key(KeyCode::Enter)),
            ReorderOutcome::Confirmed(vec!["b".to_string(), "a".to_string(), "c".to_string()])
        );
        assert_eq!(r.handle_key(&key(KeyCode::Esc)), ReorderOutcome::Cancelled);
    }

    #[test]
    fn a_key_that_means_nothing_is_pending() {
        let mut r = reorder();
        assert_eq!(
            r.handle_key(&key(KeyCode::Char('x'))),
            ReorderOutcome::Pending
        );
    }

    /// A grip press then a drag to another row then a release moves the row
    /// there, lift-and-insert, the way the agent editor's chain does.
    #[test]
    fn a_grip_drag_moves_a_row_to_where_it_is_dropped() {
        let area = Rect::new(0, 0, 90, 40);
        let mut r = reorder();
        let list = r.list_rect(area).expect("a list");
        let top = list.y;
        // Press the top row's grip (column 0 is inside the grip gutter).
        r.handle_mouse(
            &mouse(MouseEventKind::Down(MouseButton::Left), list.x, top),
            area,
        );
        assert!(r.drag.is_some(), "the grip started a drag");
        // Drag to the third row; mid-drag the order previews but is not kept.
        r.handle_mouse(
            &mouse(MouseEventKind::Drag(MouseButton::Left), list.x, top + 2),
            area,
        );
        assert_eq!(
            r.items.iter().map(|i| &i.value).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        // Release: now it is kept.
        r.handle_mouse(
            &mouse(MouseEventKind::Up(MouseButton::Left), list.x, top + 2),
            area,
        );
        assert!(r.drag.is_none());
        assert_eq!(r.values(), ["b", "c", "a"]);
    }

    /// A press off the grip gutter moves the cursor but starts no drag, so the
    /// value under it stays selectable rather than being dragged.
    #[test]
    fn a_click_off_the_grip_only_moves_the_cursor() {
        let area = Rect::new(0, 0, 90, 40);
        let mut r = reorder();
        let list = r.list_rect(area).expect("a list");
        r.handle_mouse(
            &mouse(
                MouseEventKind::Down(MouseButton::Left),
                list.x + GRIP_W + 3,
                list.y + 1,
            ),
            area,
        );
        assert_eq!(r.cursor, 1);
        assert!(r.drag.is_none(), "no drag off the grip");
    }

    /// A drag whose indices are out of range (which a real gesture cannot
    /// produce, since the mouse clamps to a row) leaves the order untouched
    /// rather than panicking on the remove.
    #[test]
    fn an_out_of_range_drag_is_a_no_op() {
        let mut terminal = Terminal::new(TestBackend::new(90, 30)).expect("backend");
        let mut r = reorder();
        r.drag = Some(Drag { from: 0, to: 99 });
        terminal
            .draw(|f| r.draw(f, f.area()))
            .expect("draw survives it");
        // The release applies it too, and finds nothing to move.
        r.handle_mouse(
            &mouse(MouseEventKind::Up(MouseButton::Left), 1, 1),
            Rect::new(0, 0, 90, 40),
        );
        assert_eq!(r.values(), ["a", "b", "c"]);
    }

    #[test]
    fn the_wheel_moves_the_cursor() {
        let area = Rect::new(0, 0, 90, 40);
        let mut r = reorder();
        r.handle_mouse(&mouse(MouseEventKind::ScrollDown, 1, 1), area);
        assert_eq!(r.cursor, 1);
        r.handle_mouse(&mouse(MouseEventKind::ScrollUp, 1, 1), area);
        assert_eq!(r.cursor, 0);
    }

    /// A drag event with no drag in progress, and a press below the last row,
    /// change nothing - the guards the live gesture leans on.
    #[test]
    fn a_stray_drag_or_a_click_past_the_end_does_nothing() {
        let area = Rect::new(0, 0, 90, 40);
        let mut r = reorder();
        assert_eq!(
            r.handle_mouse(&mouse(MouseEventKind::Drag(MouseButton::Left), 1, 1), area),
            ReorderOutcome::Pending
        );
        assert!(r.drag.is_none());
        // A press far below the list is not a row.
        r.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 1, 38), area);
        assert_eq!(r.cursor, 0);
        // A release with nothing held is a no-op.
        assert_eq!(
            r.handle_mouse(&mouse(MouseEventKind::Up(MouseButton::Left), 1, 1), area),
            ReorderOutcome::Pending
        );
        // A middle button does nothing.
        assert_eq!(
            r.handle_mouse(
                &mouse(MouseEventKind::Down(MouseButton::Middle), 1, 1),
                area
            ),
            ReorderOutcome::Pending
        );
    }

    #[test]
    fn row_at_and_on_grip_agree_with_the_drawn_list() {
        let area = Rect::new(0, 0, 90, 40);
        let r = reorder();
        let list = r.list_rect(area).expect("a list");
        assert_eq!(r.row_at(area, list.y), Some(0));
        assert_eq!(r.row_at(area, list.y + 2), Some(2));
        assert_eq!(r.row_at(area, list.y.saturating_sub(1)), None);
        assert!(r.on_grip(area, list.x));
        assert!(!r.on_grip(area, list.x + GRIP_W + 1));
    }

    /// A window too short to draw the list has no rows and no grip to hit.
    #[test]
    fn a_window_too_short_has_nothing_to_click() {
        let area = Rect::new(0, 0, 60, 4);
        let r = reorder();
        assert_eq!(r.list_rect(area), None);
        assert_eq!(r.row_at(area, 1), None);
        assert!(!r.on_grip(area, 1));
    }

    #[test]
    fn it_draws_the_rows_and_a_drag_preview() {
        let mut terminal = Terminal::new(TestBackend::new(90, 30)).expect("backend");
        let mut r = reorder();
        // Mid-drag: hold the top row over the last position.
        r.drag = Some(Drag { from: 0, to: 2 });
        terminal.draw(|f| r.draw(f, f.area())).expect("draw");
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("Provider priority"), "{text}");
        assert!(text.contains('⠿'), "the grip is drawn: {text}");
        // The held row previews at the bottom: rows read 1. b, 2. c, 3. a.
        let b_at = text.find("1. b").expect("row 1");
        let c_at = text.find("2. c").expect("row 2");
        let a_at = text.find("3. a").expect("row 3");
        assert!(
            b_at < c_at && c_at < a_at,
            "the held 'a' previews last: {text}"
        );
    }
}
