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
use unicode_width::UnicodeWidthStr;

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
    // Hints are in priority order: what does not fit the width falls off the
    // end, and an ellipsis says so (`?` always has the full list).
    let width = if bordered {
        area.width.saturating_sub(2)
    } else {
        area.width
    } as usize;
    let used = spans.iter().map(|s| s.content.width()).sum::<usize>();
    let shown = hints_that_fit(hints, width.saturating_sub(used));
    for (i, h) in hints.iter().take(shown).enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(C_DIM)));
        }
        spans.push(Span::styled(h.key, Style::default().fg(C_MUTED)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(h.label, Style::default().fg(C_DIM)));
    }
    if shown < hints.len() {
        spans.push(Span::styled(" …", Style::default().fg(C_DIM)));
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

/// How many leading hints fit in `width` cells, ellipsis included when
/// not all of them do.
fn hints_that_fit(hints: &[Hint], width: usize) -> usize {
    let cost = |i: usize, h: &Hint| {
        let sep = if i > 0 { 3 } else { 0 };
        sep + h.key.width() + 1 + h.label.width()
    };
    let total: usize = hints.iter().enumerate().map(|(i, h)| cost(i, h)).sum();
    if total <= width {
        return hints.len();
    }
    // Leave two cells for the ellipsis.
    let mut used = 0;
    let mut shown = 0;
    for (i, h) in hints.iter().enumerate() {
        let next = used + cost(i, h);
        if next + 2 > width {
            break;
        }
        used = next;
        shown = i + 1;
    }
    shown
}

/// Cut a `[key] label  [key] label …` line to `width` cells, dropping whole
/// pairs from the middle so the first pairs and the last `keep_tail` pairs
/// (help and back, usually) stay, with an ellipsis where the rest went.
///
/// The line is spans in `[key]`, ` label  ` order, as the dashboard's help
/// bars build them; a leading message span is not a pair and is kept.
pub(crate) fn fit_help_line(
    mut line: Line<'static>,
    width: usize,
    keep_tail: usize,
) -> Line<'static> {
    let total = |line: &Line<'static>| line.spans.iter().map(|s| s.content.width()).sum::<usize>();
    if total(&line) <= width {
        return line;
    }
    let ellipsis = Span::styled("…  ", Style::default().fg(C_DIM));
    // Pairs are (key, label) spans; the tail pairs are the last 2*keep_tail
    // spans. Drop the pair just before the tail until it fits.
    let tail = 2 * keep_tail;
    while line.spans.len() > tail + 2 {
        let cut_at = line.spans.len().saturating_sub(tail + 2);
        line.spans.drain(cut_at..cut_at + 2);
        let mut with_mark = line.clone();
        with_mark.spans.insert(cut_at, ellipsis.clone());
        if total(&with_mark) <= width {
            return with_mark;
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_terminal;
    use crate::tui::theme::C_WARN;

    #[test]
    fn hints_that_do_not_fit_fall_off_the_end_with_an_ellipsis() {
        let hints = [
            hint("esc", "close"),
            hint("enter", "open"),
            hint("f", "fit"),
            hint("r", "rotate"),
        ];
        // "esc close · enter open · f fit · r rotate" is 41 cells.
        assert_eq!(hints_that_fit(&hints, 41), 4);
        assert_eq!(
            hints_that_fit(&hints, 40),
            3,
            "the last one, plus the ellipsis, is one too many"
        );
        assert_eq!(hints_that_fit(&hints, 31), 2);
        assert_eq!(hints_that_fit(&hints, 23), 1);
        assert_eq!(hints_that_fit(&hints, 5), 0);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(30, 1)).unwrap();
        terminal
            .draw(|f| draw_hint_bar(f, f.area(), None, &hints, false))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.starts_with("esc close · enter open …"), "{text:?}");
        assert!(!text.contains("rotate"), "{text:?}");
        // With a border the inside is two cells narrower.
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(44, 3)).unwrap();
        terminal
            .draw(|f| draw_hint_bar(f, f.area(), None, &hints, true))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("r rotate"), "{text:?}");
    }

    #[test]
    fn a_help_line_is_cut_from_the_middle_keeping_its_tail() {
        let pair = |k: &'static str, l: &'static str| vec![Span::raw(k), Span::raw(l)];
        let mut spans = Vec::new();
        for (k, l) in [
            ("[a]", " one  "),
            ("[b]", " two  "),
            ("[c]", " three  "),
            ("[?]", " help  "),
            ("[Esc]", " back"),
        ] {
            spans.extend(pair(k, l));
        }
        let line = Line::from(spans);
        let flat = |l: &Line<'static>| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        // Wide enough: untouched.
        assert_eq!(
            flat(&fit_help_line(line.clone(), 60, 2)),
            "[a] one  [b] two  [c] three  [?] help  [Esc] back"
        );
        // Too narrow: pairs go from before the tail, the tail stays.
        assert_eq!(
            flat(&fit_help_line(line.clone(), 41, 2)),
            "[a] one  [b] two  …  [?] help  [Esc] back"
        );
        assert_eq!(
            flat(&fit_help_line(line.clone(), 32, 2)),
            "[a] one  …  [?] help  [Esc] back"
        );
        // Nothing left to drop but the head: it stays as it is, clipped by
        // the widget.
        assert_eq!(
            flat(&fit_help_line(line.clone(), 10, 2)),
            "[a] one  [?] help  [Esc] back"
        );
        // Without a tail to keep, the drop runs to the end.
        assert_eq!(flat(&fit_help_line(line, 12, 0)), "[a] one  …  ");
    }

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
