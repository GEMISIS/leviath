//! The crate's one help overlay: sections of `(key, description)` pairs.
//!
//! Surfaces build their overlay from the same tables that drive their key
//! handling, so the help text cannot drift from the real bindings. Dismissal
//! is deliberate (`Esc`, `q`, `?`, or Enter) rather than "any key" - a user
//! reading help should not trigger an action underneath by accident.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};

use super::popup::{centered, popup_frame};
use crate::tui::theme::{C_ACCENT, C_BORDER_FOCUS, C_DIM, C_MUTED, C_WHITE};

/// A titled group of key bindings shown in the help overlay.
pub(crate) struct HelpSection {
    pub(crate) title: &'static str,
    pub(crate) entries: Vec<(&'static str, &'static str)>,
}

/// True when `key` closes the help overlay. Every other key is ignored while
/// help is open - never executed underneath.
pub(crate) fn dismisses_help(key: &KeyEvent) -> bool {
    matches!(
        key.code,
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter
    )
}

/// Handle one key while the overlay is open. Returns true when it closed.
///
/// Scrolling lives here rather than in each surface so the overlay behaves the
/// same everywhere, and so a key that scrolls can never be mistaken for one
/// that acts on the screen underneath.
pub(crate) fn handle_help_key(key: &KeyEvent, offset: &Cell<usize>) -> bool {
    if dismisses_help(key) {
        offset.set(0);
        return true;
    }
    let step = |by: usize| offset.set(offset.get().saturating_add(by));
    match key.code {
        KeyCode::Down | KeyCode::Char('j') => step(1),
        KeyCode::Up | KeyCode::Char('k') => offset.set(offset.get().saturating_sub(1)),
        KeyCode::PageDown => step(10),
        KeyCode::PageUp => offset.set(offset.get().saturating_sub(10)),
        KeyCode::Home => offset.set(0),
        // Clamped to the real end by the next draw, which is the only place
        // that knows how tall the overlay came out.
        KeyCode::End => offset.set(usize::MAX),
        _ => {}
    }
    false
}

/// Draw the overlay centered in `area` (pass the whole frame area), scrolled
/// to `offset` and clamping it to what actually fits.
///
/// The clamp is written back through the `Cell` so the next key press starts
/// from a real position: without it, holding a page key runs the offset far
/// past the end and every press back up does nothing visible.
///
/// It scrolls because it has to. Help that lists a screen's keys is longer than
/// a short terminal, and an overlay that silently stops at the bottom of the
/// pane hides exactly the keys somebody opened it to find.
pub(crate) fn draw_help(
    frame: &mut Frame,
    area: Rect,
    sections: &[HelpSection],
    offset: &Cell<usize>,
) {
    let popup = centered(64, 80, area);
    let inner = popup_frame(frame, popup, "Help", C_BORDER_FOCUS);
    // The closing hint is pinned rather than scrolled: it is the one line a
    // reader needs when they have finished reading.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let mut lines = Vec::new();
    for (i, section) in sections.iter().enumerate() {
        if i > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            section.title,
            Style::default().fg(C_ACCENT),
        )));
        for (key, description) in &section.entries {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key:<18}"), Style::default().fg(C_MUTED)),
                Span::styled(*description, Style::default().fg(C_WHITE)),
            ]));
        }
    }

    let height = rows[0].height as usize;
    let max = lines.len().saturating_sub(height);
    let scroll = offset.get().min(max);
    offset.set(scroll);
    frame.render_widget(
        Paragraph::new(lines.clone()).scroll((scroll.min(u16::MAX as usize) as u16, 0)),
        rows[0],
    );

    let hint = match max {
        0 => "esc closes  (q, ? and enter too)",
        _ => "↑↓ scroll · esc closes  (q, ? and enter too)",
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(C_DIM)))),
        rows[1],
    );

    if max > 0 {
        let mut state = ScrollbarState::new(max).position(scroll);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓")),
            popup.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut state,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_terminal;
    use crossterm::event::KeyModifiers;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn only_the_deliberate_dismiss_keys_close_help() {
        assert!(dismisses_help(&press(KeyCode::Esc)));
        assert!(dismisses_help(&press(KeyCode::Char('q'))));
        assert!(dismisses_help(&press(KeyCode::Char('?'))));
        assert!(dismisses_help(&press(KeyCode::Enter)));

        assert!(!dismisses_help(&press(KeyCode::Char('x'))));
        assert!(!dismisses_help(&press(KeyCode::Char(' '))));
        assert!(!dismisses_help(&press(KeyCode::Up)));
    }

    #[test]
    fn draw_help_renders_sections_keys_and_the_dismiss_hint() {
        let sections = [
            HelpSection {
                title: "Navigate",
                entries: vec![("↑ ↓ / k j", "move"), ("enter", "open")],
            },
            HelpSection {
                title: "Actions",
                entries: vec![("x", "kill (asks first)")],
            },
        ];
        let mut terminal = test_terminal();
        terminal
            .draw(|frame| draw_help(frame, frame.area(), &sections, &Cell::new(0)))
            .unwrap();

        let text = terminal.backend().text();
        assert!(text.contains(" Help "));
        assert!(text.contains("Navigate"));
        assert!(text.contains("Actions"));
        assert!(text.contains("↑ ↓ / k j"));
        assert!(text.contains("kill (asks first)"));
        assert!(text.contains("esc closes"));
    }

    /// Every key the overlay answers, and the one thing it must not do:
    /// scroll keys are not dismissals, so reading help cannot close it.
    #[test]
    fn the_overlay_scrolls_on_its_own_keys_and_closes_only_on_the_deliberate_ones() {
        let offset = Cell::new(0);

        assert!(!handle_help_key(&press(KeyCode::Down), &offset));
        assert_eq!(offset.get(), 1);
        assert!(!handle_help_key(&press(KeyCode::Char('j')), &offset));
        assert_eq!(offset.get(), 2);
        assert!(!handle_help_key(&press(KeyCode::Up), &offset));
        assert_eq!(offset.get(), 1);
        assert!(!handle_help_key(&press(KeyCode::Char('k')), &offset));
        assert_eq!(offset.get(), 0);
        // Up from the top stays at the top rather than wrapping.
        handle_help_key(&press(KeyCode::Up), &offset);
        assert_eq!(offset.get(), 0);

        handle_help_key(&press(KeyCode::PageDown), &offset);
        assert_eq!(offset.get(), 10);
        handle_help_key(&press(KeyCode::PageUp), &offset);
        assert_eq!(offset.get(), 0);
        handle_help_key(&press(KeyCode::End), &offset);
        assert_eq!(offset.get(), usize::MAX, "clamped by the next draw");
        handle_help_key(&press(KeyCode::Home), &offset);
        assert_eq!(offset.get(), 0);

        // A key that means nothing here does nothing, rather than falling
        // through to the screen underneath.
        assert!(!handle_help_key(&press(KeyCode::Char('x')), &offset));
        assert_eq!(offset.get(), 0);

        // Closing resets, so the next open starts at the top.
        handle_help_key(&press(KeyCode::PageDown), &offset);
        assert!(handle_help_key(&press(KeyCode::Esc), &offset));
        assert_eq!(offset.get(), 0);
    }

    /// Content taller than the overlay scrolls, and the offset is clamped to
    /// the real end rather than left wherever a page key put it.
    #[test]
    fn a_long_overlay_scrolls_and_clamps_its_offset() {
        let sections = vec![HelpSection {
            title: "Long",
            entries: (0..60).map(|_| ("k", "does a thing")).collect(),
        }];
        let mut terminal =
            ratatui::Terminal::new(crate::tui::TestBackendHarness::new(60, 12)).unwrap();
        let offset = Cell::new(usize::MAX);
        terminal
            .draw(|frame| draw_help(frame, frame.area(), &sections, &offset))
            .unwrap();
        assert!(
            offset.get() < usize::MAX,
            "the draw wrote back a reachable position"
        );
        assert!(offset.get() > 0, "and it is the bottom, not the top");
    }
}
