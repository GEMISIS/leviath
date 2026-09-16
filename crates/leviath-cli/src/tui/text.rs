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

/// Word-wrap `text` into rows of at most `width` display columns.
///
/// Breaks at spaces. A single word wider than a row is cut at the row edge,
/// as many times as it takes, rather than running past it. Always at least
/// one row, so a caller counting rows never sees zero for an empty note.
pub(crate) fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row = String::new();
    for word in text.split_whitespace() {
        for piece in pieces(word, width) {
            if !row.is_empty() && row.width() + 1 + piece.width() > width {
                rows.push(std::mem::take(&mut row));
            }
            if !row.is_empty() {
                row.push(' ');
            }
            row.push_str(&piece);
        }
    }
    rows.push(row);
    rows
}

/// `word` cut into runs no wider than `width` columns: one run when it fits.
fn pieces(word: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut used = 0;
    for ch in word.chars() {
        let w = ch.width().unwrap_or(0);
        if used > 0 && used + w > width {
            out.push(std::mem::take(&mut current));
            used = 0;
        }
        current.push(ch);
        used += w;
    }
    out.push(current);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_wrap_at_the_width_and_a_long_word_is_cut() {
        assert_eq!(
            wrap_words("a name, a base URL, and a key", 12),
            ["a name, a", "base URL,", "and a key"]
        );
        assert_eq!(wrap_words("short", 12), ["short"]);
        assert_eq!(
            wrap_words("x abcdefghijklmnop y", 6),
            ["x", "abcdef", "ghijkl", "mnop y"]
        );
        // Wide characters take their two cells each.
        assert_eq!(wrap_words("日本語のモデル", 6), ["日本語", "のモデ", "ル"]);
    }

    #[test]
    fn an_empty_note_is_one_empty_row_and_a_zero_width_is_one_column() {
        assert_eq!(wrap_words("", 10), [""]);
        assert_eq!(wrap_words("   ", 10), [""]);
        assert_eq!(wrap_words("ab", 0), ["a", "b"]);
    }

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
