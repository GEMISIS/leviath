//! The crate's one help overlay: sections of `(key, description)` pairs.
//!
//! Surfaces build their overlay from the same tables that drive their key
//! handling, so the help text cannot drift from the real bindings. Dismissal
//! is deliberate (`Esc`, `q`, `?`, or Enter) rather than "any key" - a user
//! reading help should not trigger an action underneath by accident.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

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

/// Draw the overlay centered in `area` (pass the whole frame area).
pub(crate) fn draw_help(frame: &mut Frame, area: Rect, sections: &[HelpSection]) {
    let popup = centered(64, 70, area);
    let inner = popup_frame(frame, popup, "Help", C_BORDER_FOCUS);

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
                Span::styled(format!("  {key:<14}"), Style::default().fg(C_MUTED)),
                Span::styled(*description, Style::default().fg(C_WHITE)),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  esc closes",
        Style::default().fg(C_DIM),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
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
            .draw(|frame| draw_help(frame, frame.area(), &sections))
            .unwrap();

        let text = terminal.backend().text();
        assert!(text.contains(" Help "));
        assert!(text.contains("Navigate"));
        assert!(text.contains("Actions"));
        assert!(text.contains("↑ ↓ / k j"));
        assert!(text.contains("kill (asks first)"));
        assert!(text.contains("esc closes"));
    }
}
