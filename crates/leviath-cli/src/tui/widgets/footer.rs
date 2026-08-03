//! The crate's one footer hint bar.
//!
//! Every surface keeps a one-line bar of `key label` hints on screen at all
//! times; this renders it consistently (keys emphasized, labels dim,
//! `·`-separated) with an optional leading status/warning message.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::tui::theme::{C_BORDER, C_DIM, C_MUTED};

/// One `key label` pair in the bar, e.g. `("space", "select")`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Hint {
    pub(crate) key: &'static str,
    pub(crate) label: &'static str,
}

/// Shorthand constructor so hint tables stay one line per hint.
pub(crate) fn hint(key: &'static str, label: &'static str) -> Hint {
    Hint { key, label }
}

/// Render the bar. `message` (colored) leads when present; `bordered` wraps
/// the line in the standard dim border (the setup wizard's footer) or leaves
/// it bare (the dashboard's bottom line).
pub(crate) fn draw_hint_bar(
    frame: &mut Frame,
    area: Rect,
    message: Option<(&str, Color)>,
    hints: &[Hint],
    bordered: bool,
) {
    let mut spans = Vec::new();
    if let Some((text, color)) = message {
        spans.push(Span::styled(text.to_string(), Style::default().fg(color)));
        spans.push(Span::raw("  "));
    }
    for (i, h) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(C_DIM)));
        }
        spans.push(Span::styled(h.key, Style::default().fg(C_MUTED)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(h.label, Style::default().fg(C_DIM)));
    }

    let paragraph = Paragraph::new(vec![Line::from(spans)]);
    if bordered {
        frame.render_widget(
            paragraph.block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(C_BORDER)),
            ),
            area,
        );
    } else {
        frame.render_widget(paragraph, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_terminal;
    use crate::tui::theme::C_WARN;

    #[test]
    fn hints_render_as_dot_separated_key_label_pairs() {
        let mut terminal = test_terminal();
        let hints = [
            hint("space", "select"),
            hint("tab", "next"),
            hint("q", "quit"),
        ];
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, frame.area().width, 3);
                draw_hint_bar(frame, area, None, &hints, true);
            })
            .unwrap();

        assert!(
            terminal
                .backend()
                .text()
                .contains("space select · tab next · q quit")
        );
    }

    #[test]
    fn a_message_leads_the_bar_and_unbordered_renders_bare() {
        let mut terminal = test_terminal();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, frame.area().width, 1);
                draw_hint_bar(
                    frame,
                    area,
                    Some(("Skipped Credentials", C_WARN)),
                    &[hint("q", "quit")],
                    false,
                );
            })
            .unwrap();

        let text = terminal.backend().text();
        assert!(text.contains("Skipped Credentials  q quit"));
        // Bare bar: the first row is content, not a border.
        assert!(!text.lines().next().unwrap().contains('─'));
    }
}
