//! Color palette, glyph constants, and spinner frames shared by every Leviath
//! terminal UI (`lev dash`, the `lev setup` wizard, and the markdown renderer).
//!
//! This is the single definition: `commands/dashboard/theme` is an alias of
//! this module and `render.rs` imports from here, rather than each surface
//! carrying its own hand-copied palette. Two surfaces drifting apart is a real
//! cost the moment a third one exists.

use ratatui::style::Color;

// ─── Color palette ──────────────────────────────────────────────────────────

pub(crate) const C_ACCENT: Color = Color::Cyan;
pub(crate) const C_SUCCESS: Color = Color::Green;
pub(crate) const C_WARN: Color = Color::Yellow;
pub(crate) const C_ERROR: Color = Color::Red;
pub(crate) const C_DIM: Color = Color::DarkGray;
pub(crate) const C_MUTED: Color = Color::Gray;
pub(crate) const C_WHITE: Color = Color::White;
pub(crate) const C_ACTIVE: Color = Color::Cyan;
/// Script-backed (custom) region kind in the dashboard's region list.
pub(crate) const C_SCRIPT: Color = Color::Magenta;
pub(crate) const C_BORDER: Color = Color::DarkGray;
pub(crate) const C_BORDER_FOCUS: Color = Color::Cyan;

/// Background tint for fenced code blocks in the markdown renderer.
pub(crate) const C_CODE_BG: Color = Color::Rgb(30, 30, 40);

// ─── Stage status glyphs ────────────────────────────────────────────────────

pub(crate) const GLYPH_PENDING: &str = "○";
pub(crate) const GLYPH_ACTIVE: &str = "●";
pub(crate) const GLYPH_WAITING: &str = "⏸";
pub(crate) const GLYPH_COMPLETE: &str = "✓";
pub(crate) const GLYPH_ERROR: &str = "✗";

// ─── Spinner frames ─────────────────────────────────────────────────────────

pub(crate) const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

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
    fn code_background_is_not_a_foreground_color() {
        // The code-block tint must stay distinguishable from the text drawn on
        // top of it, or fenced blocks render as a solid unreadable slab.
        assert_ne!(C_CODE_BG, C_WHITE);
        assert_ne!(C_CODE_BG, C_MUTED);
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
