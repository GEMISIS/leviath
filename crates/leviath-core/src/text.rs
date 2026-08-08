//! Cutting a `&str` at a byte offset without splitting a character.
//!
//! Rust panics on `&s[..n]` when `n` lands inside a multi-byte character, and
//! this workspace slices strings at fixed byte budgets in a lot of places:
//! script-tool I/O caps, ACP frame chunking, region seed truncation, dashboard
//! column fitting, log previews. Every one of those had grown its own
//! `while !s.is_char_boundary(end) { end -= 1 }` loop with its own comment
//! explaining why - and two sites (`lev test`'s response preview and `lev
//! setup`'s key redactor) never grew one at all and panicked on any emoji.
//!
//! That failure has happened for real: a byte cut-off through a flag emoji
//! inside a Rhai host function double-panicked and aborted the whole daemon.
//! Keeping the walk-back in one tested place means a new truncation site cannot
//! forget it. The workspace denies `clippy::string_slice` with no exceptions, so
//! reaching for a raw `&s[..n]` instead of these helpers is a compile error.
//!
//! [`substring`] and [`split_at_boundary`] are the general replacements, and
//! both are *total*: no combination of offsets makes either panic. That matters
//! more than it sounds. The proof obligation on `&s[a..b]` is real but it is
//! discharged by reading, and the sites that need it most are byte-offset
//! scanners walking text nobody in this repo wrote - fetched HTML, a model's
//! fenced output, an SSE frame off the wire. Those are exactly the places where
//! a careful reading is least likely to be right, and where being wrong took
//! the daemon down. Clamping is a worse answer than a correct index and a much
//! better one than an abort.

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

/// The text of `s` between two byte offsets, with both ends walked back to a
/// char boundary and clamped into the string.
///
/// This is the workspace's substitute for `&s[a..b]`, and it is total: there is
/// no offset, ordering or overflow of the two arguments that can make it panic.
/// A range running off the end yields what is there, and a backwards range
/// yields nothing. Callers that have *searched* for their offsets get the exact
/// slice they asked for, because a `find` hit is already a boundary; callers
/// that computed one get the nearest cut that does not split a character.
///
/// That totality is the point. A scanner walking byte offsets through text it
/// did not author - HTML from a fetch, a model's fenced output, an SSE frame -
/// cannot be read closely enough to prove every offset correct, and the failure
/// mode for getting one wrong used to be aborting the daemon.
pub fn substring(s: &str, start: usize, end: usize) -> &str {
    let end = floor_char_boundary(s, end);
    let start = floor_char_boundary(s, start.min(end));
    // Both bounds are now char boundaries at or inside `s`, so the range is
    // always valid and the fallback is unreachable. It is spelled out anyway so
    // that a later change to the clamping above cannot reintroduce a panic.
    s.get(start..end).unwrap_or("")
}

/// `s` cut in two at `mid`, walked back to a char boundary.
///
/// The pair form of [`substring`], for scanners that need both the text before
/// an offset and the text after it. `mid` past the end puts everything in the
/// first half.
pub fn split_at_boundary(s: &str, mid: usize) -> (&str, &str) {
    let mid = floor_char_boundary(s, mid);
    (substring(s, 0, mid), substring(s, mid, s.len()))
}

/// `&s[..max]`, backed off to the nearest char boundary at or before `max`.
///
/// Returns all of `s` when `max` reaches the end. Callers append their own
/// ellipsis or truncation marker - this only cuts.
pub fn truncate_at_boundary(s: &str, max: usize) -> &str {
    substring(s, 0, max)
}

/// Smallest byte index `>= min` that is a char boundary in `s`.
///
/// The mirror of [`floor_char_boundary`], for cutting the *end* of a window.
/// The walk-forward always terminates: `s.len()` is a boundary in every string.
pub fn ceil_char_boundary(s: &str, min: usize) -> usize {
    let mut start = min.min(s.len());
    while !s.is_char_boundary(start) {
        start += 1;
    }
    start
}

/// A window of `s` around the byte offset `at`, reaching `radius` bytes either
/// side, with `…` marking each end that was cut.
///
/// For showing *why* something matched: a search hit deep in a megabyte of
/// transcript is only useful with the text around it, and neither existing
/// helper gives that - [`truncate_at_boundary`] only takes a prefix.
///
/// Both ends are moved outward to char boundaries rather than inward, so the
/// window never loses a character that was inside the requested radius, and the
/// match itself cannot be clipped by a boundary walk. `at` past the end clamps,
/// so a stale offset yields a short window instead of a panic.
pub fn snippet_around(s: &str, at: usize, radius: usize) -> String {
    let at = at.min(s.len());
    let start = floor_char_boundary(s, at.saturating_sub(radius));
    let end = ceil_char_boundary(s, (at + radius).min(s.len()));
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(substring(s, start, end));
    if end < s.len() {
        out.push('…');
    }
    out
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

/// Substitute `{name}` placeholders in a template.
///
/// Plain sequential `str::replace`, the same scheme `CompactionConfig`'s
/// `user_prompt` uses for `{content}` / `{region_name}` - not a template
/// language. Placeholders absent from `vars` pass through untouched, so a
/// nudge or required-region message can contain literal braces without
/// escaping as long as they don't collide with a supported name.
pub fn interpolate(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (name, value) in vars {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    out
}

#[cfg(test)]
mod snippet_tests {
    use super::*;

    const HAY: &str = "the quick brown fox jumps over the lazy dog";

    #[test]
    fn a_window_in_the_middle_is_elided_at_both_ends() {
        let at = HAY.find("fox").unwrap();
        let out = snippet_around(HAY, at, 6);
        assert!(out.starts_with('…'));
        assert!(out.ends_with('…'));
        assert!(out.contains("fox"));
    }

    #[test]
    fn a_window_at_the_edges_is_not_elided_there() {
        assert!(!snippet_around(HAY, 0, 5).starts_with('…'));
        assert!(!snippet_around(HAY, HAY.len(), 5).ends_with('…'));
    }

    #[test]
    fn a_radius_covering_everything_returns_the_whole_string_unmarked() {
        assert_eq!(snippet_around(HAY, 10, 1000), HAY);
    }

    /// The reason this lives here rather than at a call site: a byte offset
    /// landing inside a multi-byte character used to abort the daemon.
    #[test]
    fn multi_byte_characters_are_never_split() {
        let hay = "aaa🇯🇵🎉bbb needle ccc🚀ddd";
        let at = hay.find("needle").unwrap();
        // Every radius walks the ends over the emoji in both directions.
        for radius in 0..hay.len() + 4 {
            let out = snippet_around(hay, at, radius);
            assert!(out.chars().all(|c| c != '\u{FFFD}'));
            if radius >= "needle".len() {
                assert!(out.contains("needle"));
            }
        }
    }

    #[test]
    fn an_offset_past_the_end_clamps_instead_of_panicking() {
        let out = snippet_around(HAY, HAY.len() + 500, 4);
        assert!(out.starts_with('…'));
        assert!(!out.ends_with('…'));
    }

    #[test]
    fn an_empty_haystack_yields_an_empty_snippet() {
        assert_eq!(snippet_around("", 0, 10), "");
        assert_eq!(snippet_around("", 7, 10), "");
    }

    #[test]
    fn ceil_char_boundary_walks_forward_and_clamps() {
        let s = "a🎉b";
        assert_eq!(ceil_char_boundary(s, 0), 0);
        // Bytes 2..4 are inside the emoji; the next boundary is 5.
        assert_eq!(ceil_char_boundary(s, 2), 5);
        assert_eq!(ceil_char_boundary(s, 900), s.len());
    }
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
    fn interpolate_replaces_known_placeholders_and_keeps_the_rest() {
        // Present, repeated, and absent placeholders in one template.
        assert_eq!(
            interpolate(
                "populate {region} - yes, {region} - in stage {stage} {unknown}",
                &[("region", "plan"), ("stage", "design")]
            ),
            "populate plan - yes, plan - in stage design {unknown}"
        );
        // No vars: the template passes through unchanged.
        assert_eq!(interpolate("no placeholders", &[]), "no placeholders");
    }

    #[test]
    fn floor_char_boundary_clamps_and_walks_back() {
        // Past the end clamps to the length.
        assert_eq!(floor_char_boundary("abc", 99), 3);
        // Already a boundary - unchanged.
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

    #[test]
    fn substring_is_total() {
        assert_eq!(substring("abcdef", 2, 4), "cd");
        assert_eq!(substring("abcdef", 0, 6), "abcdef");
        // Both ends walk back off a multi-byte character rather than panicking.
        assert_eq!(substring("a🎉b", 1, 4), "");
        assert_eq!(substring("a🎉b", 0, 3), "a");
        assert_eq!(substring("a🎉b", 1, 5), "🎉");
        // No pair of arguments panics: past the end, backwards, and saturated.
        assert_eq!(substring("abc", 1, 99), "bc");
        assert_eq!(substring("abc", 99, 99), "");
        assert_eq!(substring("abc", 2, 1), "");
        assert_eq!(substring("abc", usize::MAX, usize::MAX), "");
        assert_eq!(substring("", 3, 9), "");
    }

    #[test]
    fn split_at_boundary_halves_rejoin_to_the_input() {
        assert_eq!(split_at_boundary("abcdef", 2), ("ab", "cdef"));
        assert_eq!(split_at_boundary("abc", 0), ("", "abc"));
        // A cut inside the emoji lands before it, so nothing is lost or doubled.
        assert_eq!(split_at_boundary("a🎉b", 3), ("a", "🎉b"));
        // Past the end puts everything in the first half.
        assert_eq!(split_at_boundary("abc", 99), ("abc", ""));
        assert_eq!(split_at_boundary("", 4), ("", ""));
        for mid in 0..=10 {
            let (head, tail) = split_at_boundary("a🎉bc", mid);
            assert_eq!(format!("{head}{tail}"), "a🎉bc", "lost text at mid={mid}");
        }
    }
}
