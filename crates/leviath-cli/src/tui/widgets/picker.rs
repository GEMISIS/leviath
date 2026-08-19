//! A full-screen chooser: a list, filtered as you type, with a line or two
//! saying what the choice decides.
//!
//! The setup wizard grew this for its Defaults screen, where the arrows
//! cycled a field's values in place, which is fine for three providers and
//! hopeless for eighty models. The dashboard's agent editor picks models,
//! stages, regions and tools the same way, so the chooser lives here and
//! both use it. A multi-select mode (space toggles, enter is done) serves
//! the tools list.

use std::collections::BTreeSet;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use super::line_edit::{EditOutcome, LineEdit};
use super::popup::{centered, popup_frame};
use crate::tui::theme::{C_ACCENT, C_ACTIVE, C_BORDER_FOCUS, C_DIM, C_MUTED, C_WARN, C_WHITE};

/// Rows a page key moves.
const PAGE: isize = 8;

/// One row of a [`Picker`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PickerOption {
    /// The value that would be written.
    pub(crate) value: String,
    /// Where it came from, or what it is, in a few words.
    pub(crate) detail: String,
}

/// What a key or click did to the chooser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PickerOutcome {
    /// Still choosing.
    Pending,
    /// One option chosen (single-select): its index into `options`.
    Chosen(usize),
    /// Done choosing (multi-select): the chosen indices, in list order.
    ChosenMany(Vec<usize>),
    /// Closed without choosing.
    Cancelled,
}

/// The chooser's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Picker {
    /// Its heading.
    pub(crate) title: String,
    /// What the value actually decides, and what it does not.
    pub(crate) explain: Vec<String>,
    /// The search box.
    pub(crate) query: LineEdit,
    /// Every option, in the caller's order.
    pub(crate) options: Vec<PickerOption>,
    /// Cursor into the *filtered* list, not into `options`.
    pub(crate) cursor: usize,
    /// Multi-select: the chosen indices into `options`. `None` = one choice.
    pub(crate) multi: Option<BTreeSet<usize>>,
}

impl Picker {
    /// A single-select chooser open on `cursor` (an index into `options`,
    /// so a list opens on its current value rather than at the top).
    pub(crate) fn new(
        title: impl Into<String>,
        explain: Vec<String>,
        options: Vec<PickerOption>,
        cursor: usize,
    ) -> Self {
        Self {
            title: title.into(),
            explain,
            query: LineEdit::new(String::new(), false),
            options,
            cursor,
            multi: None,
        }
    }

    /// The options matching the query, as indices into `options`.
    ///
    /// Every whitespace-separated term has to appear somewhere in the row, so
    /// "claude sonnet" finds the model whichever order the id puts them in,
    /// and a term can match the detail as easily as the value.
    pub(crate) fn matches(&self) -> Vec<usize> {
        let query = self.query.value().to_lowercase();
        let terms: Vec<&str> = query.split_whitespace().collect();
        self.options
            .iter()
            .enumerate()
            .filter(|(_, option)| {
                let haystack = format!("{} {}", option.value, option.detail).to_lowercase();
                terms.iter().all(|term| haystack.contains(term))
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// Which option the cursor is on, if the filter left anything.
    pub(crate) fn selected(&self) -> Option<usize> {
        self.matches().get(self.cursor).copied()
    }

    /// Move within the filtered list, clamped to it.
    ///
    /// Clamping rather than wrapping: at eighty models, wrapping from the top
    /// to the bottom looks like the list jumped rather than moved.
    pub(crate) fn move_cursor(&mut self, delta: isize) {
        let count = self.matches().len();
        let next = self.cursor as isize + delta;
        self.cursor = next.clamp(0, count.saturating_sub(1) as isize) as usize;
    }

    /// Whether an option is chosen (multi-select).
    pub(crate) fn is_chosen(&self, index: usize) -> bool {
        self.multi.as_ref().is_some_and(|m| m.contains(&index))
    }

    /// Flip the option under the cursor (multi-select).
    fn toggle_selected(&mut self) {
        if let Some(index) = self.selected()
            && let Some(multi) = self.multi.as_mut()
            && !multi.remove(&index)
        {
            multi.insert(index);
        }
    }

    fn done(&self) -> PickerOutcome {
        match &self.multi {
            Some(multi) => PickerOutcome::ChosenMany(multi.iter().copied().collect()),
            None => match self.selected() {
                Some(index) => PickerOutcome::Chosen(index),
                // An empty filter has nothing to choose; closing without a
                // change is the only honest outcome.
                None => PickerOutcome::Cancelled,
            },
        }
    }

    /// Keys while the chooser is open.
    ///
    /// Everything that is not navigation goes to the search box, so letters
    /// type rather than acting: `q` in a chooser means the user is looking
    /// for Qwen, and quitting instead would be indefensible.
    pub(crate) fn handle_key(&mut self, key: &KeyEvent) -> PickerOutcome {
        match key.code {
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-PAGE),
            KeyCode::PageDown => self.move_cursor(PAGE),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.move_cursor(isize::MAX),
            KeyCode::Char(' ') if self.multi.is_some() => self.toggle_selected(),
            _ => {
                match self.query.handle_key(key) {
                    EditOutcome::Commit => return self.done(),
                    // Esc closes without choosing, leaving the field as it was.
                    EditOutcome::Cancel => return PickerOutcome::Cancelled,
                    EditOutcome::Pending => {}
                }
                // The filter just changed under the cursor, so a selection
                // that has been filtered away must not linger off the end.
                self.move_cursor(0);
            }
        }
        PickerOutcome::Pending
    }

    /// The mouse while the chooser is open: the wheel moves within the list,
    /// a click on a row takes it (or, multi-select, flips it).
    ///
    /// A click outside the list is ignored rather than closing the chooser.
    /// Closing on a stray click would discard a search somebody was halfway
    /// through typing, and Esc is right there.
    pub(crate) fn handle_mouse(&mut self, mouse: &MouseEvent, area: Rect) -> PickerOutcome {
        match mouse.kind {
            MouseEventKind::ScrollDown => self.move_cursor(1),
            MouseEventKind::ScrollUp => self.move_cursor(-1),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(row) = self.row_at(area, mouse.row) {
                    self.cursor = row;
                    if self.multi.is_some() {
                        self.toggle_selected();
                    } else {
                        return self.done();
                    }
                }
            }
            _ => {}
        }
        PickerOutcome::Pending
    }

    /// The chooser's split: prose, search box, then the list with the rest.
    fn layout(&self, inner: Rect) -> std::rc::Rc<[Rect]> {
        let explain = (self.explain.len() as u16 + 2).min(inner.height.saturating_sub(4));
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(explain),
                Constraint::Length(2),
                Constraint::Min(1),
            ])
            .split(inner)
    }

    /// Which option a click landed on, as an index into the filtered list.
    /// Shares the layout with the drawing, so a click cannot resolve against
    /// rows that were not on screen.
    pub(crate) fn row_at(&self, area: Rect, row: u16) -> Option<usize> {
        let popup = centered(80, 88, area);
        // What `popup_frame` leaves after its border.
        let inner = Block::default().borders(Borders::ALL).inner(popup);
        if inner.height < 4 {
            return None;
        }
        let list = self.layout(inner)[2];
        if row < list.y || row >= list.y + list.height {
            return None;
        }
        let height = list.height as usize;
        let offset = self.cursor.saturating_sub(height.saturating_sub(1));
        let position = offset + (row - list.y) as usize;
        (position < self.matches().len()).then_some(position)
    }

    /// Draw the chooser over everything else.
    ///
    /// It takes most of the window rather than a small popup: the whole
    /// complaint it answers is that a long list read through one line is
    /// unreadable, and the prose above it is the other half of the answer.
    pub(crate) fn draw(&self, frame: &mut Frame, area: Rect) {
        let popup = centered(80, 88, area);
        let inner = popup_frame(frame, popup, &self.title, C_BORDER_FOCUS);
        let chunks = self.layout(inner);

        let mut lines: Vec<Line<'static>> = self
            .explain
            .iter()
            .map(|text| Line::from(Span::styled(text.clone(), Style::default().fg(C_MUTED))))
            .collect();
        if self.multi.is_some() {
            lines.push(Line::from(Span::styled(
                "Space picks or drops a row; Enter keeps what is picked.",
                Style::default().fg(C_MUTED),
            )));
        }
        lines.push(Line::from(""));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), chunks[0]);

        let mut search = vec![Span::styled("Search  ", Style::default().fg(C_DIM))];
        search.extend(self.query.display_spans(true).spans);
        frame.render_widget(
            Paragraph::new(vec![Line::from(search), Line::from("")]),
            chunks[1],
        );

        let matches = self.matches();
        if matches.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "Nothing matches that.",
                    Style::default().fg(C_WARN),
                ))),
                chunks[2],
            );
            return;
        }

        let height = chunks[2].height as usize;
        // Keep the cursor in view without a stored offset: the list is rebuilt
        // every frame anyway, so the window into it is arithmetic, not state.
        let offset = self.cursor.saturating_sub(height.saturating_sub(1));
        let rows: Vec<Line<'static>> = matches
            .iter()
            .enumerate()
            .skip(offset)
            .take(height)
            .map(|(position, index)| {
                let option = &self.options[*index];
                let selected = position == self.cursor;
                let mark = match &self.multi {
                    Some(_) if self.is_chosen(*index) => "[x] ",
                    Some(_) => "[ ] ",
                    None => "",
                };
                Line::from(vec![
                    Span::styled(
                        if selected { "› " } else { "  " },
                        Style::default().fg(C_ACCENT),
                    ),
                    Span::styled(mark, Style::default().fg(C_ACCENT)),
                    Span::styled(
                        format!("{:<38}", option.value),
                        if selected {
                            Style::default().fg(C_ACTIVE).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(C_WHITE)
                        },
                    ),
                    Span::styled(option.detail.clone(), Style::default().fg(C_DIM)),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(rows), chunks[2]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn options() -> Vec<PickerOption> {
        ["alpha", "beta", "gamma", "delta"]
            .iter()
            .map(|v| PickerOption {
                value: v.to_string(),
                detail: format!("the letter {v}"),
            })
            .collect()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        }
    }

    fn rendered(picker: &Picker, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| picker.draw(f, f.area())).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect()
    }

    #[test]
    fn typing_filters_and_enter_chooses() {
        let mut p = Picker::new("Pick", vec!["what it decides".into()], options(), 1);
        assert_eq!(p.selected(), Some(1));
        for c in "ta".chars() {
            assert_eq!(p.handle_key(&key(KeyCode::Char(c))), PickerOutcome::Pending);
        }
        assert_eq!(p.matches(), [1, 3], "beta and delta");
        assert_eq!(p.cursor, 1);
        assert_eq!(p.handle_key(&key(KeyCode::Down)), PickerOutcome::Pending);
        assert_eq!(
            p.handle_key(&key(KeyCode::Down)),
            PickerOutcome::Pending,
            "clamped"
        );
        assert_eq!(p.handle_key(&key(KeyCode::Enter)), PickerOutcome::Chosen(3));
        assert_eq!(p.handle_key(&key(KeyCode::Home)), PickerOutcome::Pending);
        assert_eq!(p.cursor, 0);
        assert_eq!(p.handle_key(&key(KeyCode::End)), PickerOutcome::Pending);
        assert_eq!(p.cursor, 1);
        assert_eq!(p.handle_key(&key(KeyCode::PageUp)), PickerOutcome::Pending);
        assert_eq!(
            p.handle_key(&key(KeyCode::PageDown)),
            PickerOutcome::Pending
        );
        assert_eq!(p.handle_key(&key(KeyCode::Up)), PickerOutcome::Pending);
        assert_eq!(p.handle_key(&key(KeyCode::Esc)), PickerOutcome::Cancelled);
        // Nothing matches: Enter cannot choose.
        for c in "zz".chars() {
            p.handle_key(&key(KeyCode::Char(c)));
        }
        assert!(p.selected().is_none());
        assert_eq!(p.handle_key(&key(KeyCode::Enter)), PickerOutcome::Cancelled);
        assert!(rendered(&p, 80, 30).contains("Nothing matches that."));
    }

    #[test]
    fn multi_select_toggles_with_space_and_click() {
        let mut p = Picker::new("Tools", vec![], options(), 0);
        p.multi = Some([2].into_iter().collect());
        assert!(p.is_chosen(2));
        assert_eq!(
            p.handle_key(&key(KeyCode::Char(' '))),
            PickerOutcome::Pending
        );
        assert!(p.is_chosen(0));
        assert_eq!(
            p.handle_key(&key(KeyCode::Char(' '))),
            PickerOutcome::Pending
        );
        assert!(!p.is_chosen(0), "space again drops it");
        let area = Rect::new(0, 0, 80, 30);
        let row = (0..30)
            .find(|y| p.row_at(area, *y) == Some(1))
            .expect("beta on screen");
        assert_eq!(
            p.handle_mouse(
                &mouse(MouseEventKind::Down(MouseButton::Left), 10, row),
                area
            ),
            PickerOutcome::Pending
        );
        assert!(p.is_chosen(1));
        let text = rendered(&p, 80, 30);
        assert!(text.contains("[x] beta"), "{text}");
        assert!(text.contains("[ ] alpha"), "{text}");
        assert!(text.contains("Space picks or drops"), "{text}");
        assert_eq!(
            p.handle_key(&key(KeyCode::Enter)),
            PickerOutcome::ChosenMany(vec![1, 2])
        );
        // Space with an empty filter is nothing to toggle.
        for c in "zz".chars() {
            p.handle_key(&key(KeyCode::Char(c)));
        }
        assert_eq!(
            p.handle_key(&key(KeyCode::Char(' '))),
            PickerOutcome::Pending
        );
        assert_eq!(
            p.handle_key(&key(KeyCode::Enter)),
            PickerOutcome::ChosenMany(vec![1, 2])
        );
    }

    #[test]
    fn the_mouse_scrolls_and_clicks_a_row() {
        let mut p = Picker::new("Pick", vec!["a".into()], options(), 0);
        let area = Rect::new(0, 0, 80, 30);
        assert_eq!(
            p.handle_mouse(&mouse(MouseEventKind::ScrollDown, 1, 1), area),
            PickerOutcome::Pending
        );
        assert_eq!(p.cursor, 1);
        assert_eq!(
            p.handle_mouse(&mouse(MouseEventKind::ScrollUp, 1, 1), area),
            PickerOutcome::Pending
        );
        assert_eq!(p.cursor, 0);
        // Outside the list: ignored. A move: ignored.
        assert_eq!(
            p.handle_mouse(&mouse(MouseEventKind::Down(MouseButton::Left), 1, 0), area),
            PickerOutcome::Pending
        );
        assert_eq!(
            p.handle_mouse(&mouse(MouseEventKind::Moved, 1, 1), area),
            PickerOutcome::Pending
        );
        let row = (0..30)
            .find(|y| p.row_at(area, *y) == Some(2))
            .expect("gamma on screen");
        assert_eq!(
            p.handle_mouse(
                &mouse(MouseEventKind::Down(MouseButton::Left), 10, row),
                area
            ),
            PickerOutcome::Chosen(2)
        );
        // Too short a window has no rows; below the last row is not a row.
        assert_eq!(p.row_at(Rect::new(0, 0, 60, 5), 3), None);
        assert_eq!(p.row_at(area, 29), None);
        let text = rendered(&p, 80, 30);
        assert!(text.contains("Search"), "{text}");
        assert!(text.contains("the letter gamma"), "{text}");
    }
}
