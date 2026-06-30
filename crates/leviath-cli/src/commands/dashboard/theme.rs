//! Color palette, glyph constants, and spinner frames for the dashboard TUI.

use ratatui::style::Color;

// ─── Color palette ──────────────────────────────────────────────────────────

pub(super) const C_ACCENT: Color = Color::Cyan;
pub(super) const C_SUCCESS: Color = Color::Green;
pub(super) const C_WARN: Color = Color::Yellow;
pub(super) const C_ERROR: Color = Color::Red;
pub(super) const C_DIM: Color = Color::DarkGray;
pub(super) const C_MUTED: Color = Color::Gray;
pub(super) const C_WHITE: Color = Color::White;
pub(super) const C_ACTIVE: Color = Color::Cyan;
pub(super) const C_BORDER: Color = Color::DarkGray;
pub(super) const C_BORDER_FOCUS: Color = Color::Cyan;

// ─── Stage status glyphs ────────────────────────────────────────────────────

pub(super) const GLYPH_PENDING: &str = "○";
pub(super) const GLYPH_ACTIVE: &str = "●";
pub(super) const GLYPH_WAITING: &str = "⏸";
pub(super) const GLYPH_COMPLETE: &str = "✓";
pub(super) const GLYPH_ERROR: &str = "✗";

// ─── Spinner frames ─────────────────────────────────────────────────────────

pub(super) const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_colors_are_distinct() {
        // Verify key colors don't collide
        assert_ne!(C_ACCENT, C_ERROR);
        assert_ne!(C_SUCCESS, C_ERROR);
        assert_ne!(C_WARN, C_SUCCESS);
        assert_ne!(C_DIM, C_WHITE);
    }

    #[test]
    fn spinner_has_ten_frames() {
        assert_eq!(SPINNER.len(), 10);
        for frame in &SPINNER {
            assert!(!frame.is_empty());
        }
    }

    #[test]
    fn glyphs_are_nonempty() {
        assert!(!GLYPH_PENDING.is_empty());
        assert!(!GLYPH_ACTIVE.is_empty());
        assert!(!GLYPH_WAITING.is_empty());
        assert!(!GLYPH_COMPLETE.is_empty());
        assert!(!GLYPH_ERROR.is_empty());
    }
}
