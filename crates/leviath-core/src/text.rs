//! Cutting a `&str` at a byte offset without splitting a character.
//!
//! Rust panics on `&s[..n]` when `n` lands inside a multi-byte character, and
//! this workspace slices strings at fixed byte budgets in a lot of places:
//! script-tool I/O caps, ACP frame chunking, region seed truncation, dashboard
//! column fitting, log previews. Every one of those had grown its own
//! `while !s.is_char_boundary(end) { end -= 1 }` loop with its own comment
//! explaining why — and two sites (`lev test`'s response preview and `lev
//! setup`'s key redactor) never grew one at all and panicked on any emoji.
//!
//! That is the shape of issues #109 and #115: a byte cut-off through a flag
//! emoji inside a Rhai host function, which double-panicked and aborted the
//! whole daemon. Keeping the walk-back in one tested place means a new
//! truncation site cannot forget it. The workspace also denies
//! `clippy::string_slice`, so reaching for a raw `&s[..n]` instead of these
//! helpers is a compile error unless the boundary is proven at the call site.

/// Largest byte index `<= max` that is a char boundary in `s`.
///
/// `max` past the end clamps to `s.len()`. The walk-back always terminates:
/// byte 0 is a boundary in every string, including the empty one.
pub fn floor_char_boundary(s: &str, max: usize) -> usize {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// `&s[..max]`, backed off to the nearest char boundary at or before `max`.
///
/// Returns all of `s` when `max` reaches the end. Callers append their own
/// ellipsis or truncation marker — this only cuts.
#[expect(
    clippy::string_slice,
    reason = "floor_char_boundary returns a char boundary by construction — this is the one raw \
              slice the rest of the workspace defers to"
)]
pub fn truncate_at_boundary(s: &str, max: usize) -> &str {
    &s[..floor_char_boundary(s, max)]
}

/// The workspace's one generic token estimate: bytes divided by four,
/// rounded up.
///
/// Every context budget, eviction threshold, and truncation cap that has no
/// exact tokenizer runs on this. It was open-coded across ~30 sites with
/// three disagreeing formulas (`/4`, `/4 + 1`, `div_ceil(4)`), which meant
/// the same text could count differently on the budgeting side and the
/// truncation side of one decision. Rounding up (never 0 for non-empty text)
/// is the safe direction for a budget: overestimating spends a token of
/// headroom, underestimating overflows a window.
///
/// Provider-accuracy heuristics (e.g. the Anthropic-calibrated bytes/3.5 in
/// `leviath-providers`) are deliberately separate: they estimate a specific
/// tokenizer, this estimates "text-shaped budget units".
pub fn estimate_tokens(s: &str) -> usize {
    s.len().div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_rounds_up_and_never_zero_for_nonempty() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        // Bytes, not chars: one 3-byte character still costs one budget unit.
        assert_eq!(estimate_tokens("\u{65e5}"), 1);
    }

    #[test]
    fn floor_char_boundary_clamps_and_walks_back() {
        // Past the end clamps to the length.
        assert_eq!(floor_char_boundary("abc", 99), 3);
        // Already a boundary — unchanged.
        assert_eq!(floor_char_boundary("abc", 2), 2);
        // Zero is always a boundary, so no walk-back happens.
        assert_eq!(floor_char_boundary("日本語", 0), 0);
        // Mid-character walks back to the start of that character. '🎉' is four
        // bytes at 3..7, so 4, 5 and 6 all floor to 3.
        let s = "abc🎉";
        assert_eq!(floor_char_boundary(s, 4), 3);
        assert_eq!(floor_char_boundary(s, 6), 3);
        assert_eq!(floor_char_boundary(s, 7), 7);
        // Walking back all the way to 0 when the first character straddles the cut.
        assert_eq!(floor_char_boundary("🎉abc", 2), 0);
        // Empty string: byte 0 is a boundary, so any max clamps to 0.
        assert_eq!(floor_char_boundary("", 5), 0);
    }

    #[test]
    fn truncate_at_boundary_never_splits_a_character() {
        assert_eq!(truncate_at_boundary("abc", 99), "abc");
        assert_eq!(truncate_at_boundary("abcdef", 3), "abc");
        // The exact shape that panicked in issues #109/#115.
        assert_eq!(truncate_at_boundary("abc🎉def", 5), "abc");
        assert_eq!(truncate_at_boundary("🎉abc", 2), "");
        assert_eq!(truncate_at_boundary("", 5), "");
    }
}
