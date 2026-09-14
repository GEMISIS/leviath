//! Text that has to fit a terminal cell count.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Cut `s` to at most `max` display columns, ending in an ellipsis when it
/// was cut. Whitespace is kept: an indented label stays indented.
///
/// Measured in display columns, not bytes or characters: counting bytes cuts
/// a title full of long dashes or emoji at a third of the room it was given,
/// counting characters lets a CJK label run two cells past its column, and a
/// wide character counts here for the two cells it occupies. Every surface
/// that fits text into a column goes through this one function.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let keep = max - 1;
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > keep {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_characters_count_for_the_cells_they_occupy() {
        // Six characters, twelve cells: six columns hold two of them and the
        // ellipsis, not all six.
        assert_eq!(truncate("日本語のモデル", 6), "日本…");
        assert_eq!(truncate("日本語", 6), "日本語");
    }

    #[test]
    fn a_zero_width_room_shows_nothing() {
        assert_eq!(truncate("plan", 0), "");
        assert_eq!(truncate("plan", 1), "…");
    }

    #[test]
    fn an_indent_is_part_of_the_text() {
        assert_eq!(truncate("  from shots", 20), "  from shots");
        assert_eq!(truncate("  from shots", 8), "  from …");
    }
}
