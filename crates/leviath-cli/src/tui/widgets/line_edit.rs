//! The crate's one single-line text input: cursor movement, masking, and a
//! visible cursor cell.
//!
//! Replaces the surfaces' hand-rolled `String` push/pop editors (which had no
//! cursor at all). Multi-line editing stays on `ratatui-textarea` where it
//! already earns its keep; this covers the far more common one-line field.
//! The cursor is a char index and all splicing goes through char boundaries -
//! this crate denies `clippy::string_slice` precisely because byte-index
//! slicing of user input is how multibyte panics happen.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::tui::theme::C_WHITE;

/// What a keypress did to the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditOutcome {
    /// Still editing.
    Pending,
    /// Enter: accept the current value.
    Commit,
    /// Esc: discard the edit.
    Cancel,
}

/// A single-line editor. `masked` renders bullets unless the caller passes
/// `reveal` at draw time (the wizard's Ctrl-R).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LineEdit {
    buffer: String,
    /// Char index (not byte) of the insertion point, `0..=char_len`.
    cursor: usize,
    pub(crate) masked: bool,
}

impl LineEdit {
    pub(crate) fn new(initial: impl Into<String>, masked: bool) -> Self {
        let buffer = initial.into();
        let cursor = buffer.chars().count();
        Self {
            buffer,
            cursor,
            masked,
        }
    }

    pub(crate) fn value(&self) -> &str {
        &self.buffer
    }

    fn char_len(&self) -> usize {
        self.buffer.chars().count()
    }

    /// Byte offset of the `char_idx`-th char (or the end of the buffer).
    fn byte_at(&self, char_idx: usize) -> usize {
        self.buffer
            .char_indices()
            .nth(char_idx)
            .map_or(self.buffer.len(), |(i, _)| i)
    }

    /// Chars insert at the cursor; Backspace/Delete remove around it;
    /// `←`/`→`/Home/End move it; Enter commits; Esc cancels. Control chords
    /// and other keys are ignored, preserving the buffer.
    pub(crate) fn handle_key(&mut self, key: &KeyEvent) -> EditOutcome {
        match key.code {
            KeyCode::Enter => return EditOutcome::Commit,
            KeyCode::Esc => return EditOutcome::Cancel,
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.char_len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.char_len(),
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let start = self.byte_at(self.cursor - 1);
                    let end = self.byte_at(self.cursor);
                    self.buffer.replace_range(start..end, "");
                    self.cursor -= 1;
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.char_len() {
                    let start = self.byte_at(self.cursor);
                    let end = self.byte_at(self.cursor + 1);
                    self.buffer.replace_range(start..end, "");
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let at = self.byte_at(self.cursor);
                self.buffer.insert(at, c);
                self.cursor += 1;
            }
            _ => {}
        }
        EditOutcome::Pending
    }

    /// The field's visible content with a reversed cursor cell. Masked fields
    /// render one bullet per char unless `reveal`.
    pub(crate) fn display_spans(&self, reveal: bool) -> Line<'static> {
        let shown: String = if self.masked && !reveal {
            "•".repeat(self.char_len())
        } else {
            self.buffer.clone()
        };
        let before: String = shown.chars().take(self.cursor).collect();
        let at: String = shown.chars().skip(self.cursor).take(1).collect();
        let after: String = shown.chars().skip(self.cursor + 1).collect();
        let cursor_cell = if at.is_empty() { " ".to_string() } else { at };
        let plain = Style::default().fg(C_WHITE);
        Line::from(vec![
            Span::styled(before, plain),
            Span::styled(cursor_cell, plain.add_modifier(Modifier::REVERSED)),
            Span::styled(after, plain),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn type_str(edit: &mut LineEdit, s: &str) {
        for c in s.chars() {
            assert_eq!(
                edit.handle_key(&press(KeyCode::Char(c))),
                EditOutcome::Pending
            );
        }
    }

    #[test]
    fn typing_appends_and_the_cursor_starts_at_the_end_of_the_initial_value() {
        let mut edit = LineEdit::new("sk-", true);
        type_str(&mut edit, "abc");
        assert_eq!(edit.value(), "sk-abc");
    }

    #[test]
    fn arrows_home_and_end_move_the_insertion_point() {
        let mut edit = LineEdit::new("ad", false);
        edit.handle_key(&press(KeyCode::Left));
        type_str(&mut edit, "bc");
        assert_eq!(edit.value(), "abcd");

        edit.handle_key(&press(KeyCode::Home));
        type_str(&mut edit, "x");
        assert_eq!(edit.value(), "xabcd");

        edit.handle_key(&press(KeyCode::End));
        type_str(&mut edit, "z");
        assert_eq!(edit.value(), "xabcdz");

        // Movement clamps at both edges.
        for _ in 0..20 {
            edit.handle_key(&press(KeyCode::Left));
        }
        edit.handle_key(&press(KeyCode::Left));
        for _ in 0..20 {
            edit.handle_key(&press(KeyCode::Right));
        }
        edit.handle_key(&press(KeyCode::Right));
        assert_eq!(edit.value(), "xabcdz");
    }

    #[test]
    fn backspace_and_delete_remove_around_the_cursor() {
        let mut edit = LineEdit::new("abc", false);
        edit.handle_key(&press(KeyCode::Backspace));
        assert_eq!(edit.value(), "ab");

        edit.handle_key(&press(KeyCode::Home));
        edit.handle_key(&press(KeyCode::Delete));
        assert_eq!(edit.value(), "b");

        // At the edges both are no-ops.
        edit.handle_key(&press(KeyCode::Backspace));
        assert_eq!(edit.value(), "b");
        edit.handle_key(&press(KeyCode::End));
        edit.handle_key(&press(KeyCode::Delete));
        assert_eq!(edit.value(), "b");
    }

    #[test]
    fn multibyte_input_splices_on_char_boundaries() {
        let mut edit = LineEdit::new("", false);
        type_str(&mut edit, "héllo wörld");
        assert_eq!(edit.value(), "héllo wörld");

        // Move left to just after the multibyte 'ö' and delete it.
        for _ in 0..3 {
            edit.handle_key(&press(KeyCode::Left));
        }
        edit.handle_key(&press(KeyCode::Backspace));
        assert_eq!(edit.value(), "héllo wrld");

        // Insert a multibyte char mid-string.
        type_str(&mut edit, "ø");
        assert_eq!(edit.value(), "héllo wørld");
    }

    #[test]
    fn enter_commits_and_esc_cancels() {
        let mut edit = LineEdit::new("v", false);
        assert_eq!(edit.handle_key(&press(KeyCode::Enter)), EditOutcome::Commit);
        assert_eq!(edit.handle_key(&press(KeyCode::Esc)), EditOutcome::Cancel);
        assert_eq!(edit.value(), "v");
    }

    #[test]
    fn control_chords_and_unmapped_keys_leave_the_buffer_alone() {
        let mut edit = LineEdit::new("keep", false);
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(edit.handle_key(&ctrl_s), EditOutcome::Pending);
        assert_eq!(edit.handle_key(&press(KeyCode::F(2))), EditOutcome::Pending);
        assert_eq!(edit.value(), "keep");
    }

    #[test]
    fn display_masks_bullets_reveals_on_demand_and_shows_a_cursor_cell() {
        let edit = LineEdit::new("ab", true);
        let masked = edit.display_spans(false);
        let masked_text: String = masked.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(masked_text, "•• ");

        let revealed = edit.display_spans(true);
        let revealed_text: String = revealed.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(revealed_text, "ab ");

        // Unmasked fields always show plain text; mid-string cursor renders
        // the char under it as the cursor cell instead of a trailing space.
        let mut plain = LineEdit::new("xy", false);
        plain.handle_key(&press(KeyCode::Home));
        let line = plain.display_spans(false);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "xy");
        assert_eq!(line.spans[1].content.as_ref(), "x");
    }
}
