//! Tests for the search matching primitives.

use super::*;

#[test]
fn a_match_reports_the_offset_it_was_found_at() {
    assert_eq!(
        contains_ignore_ascii_case(b"hello world", b"world"),
        Some(6)
    );
    assert_eq!(
        contains_ignore_ascii_case(b"hello world", b"hello"),
        Some(0)
    );
}

#[test]
fn case_is_ignored_on_both_sides() {
    assert_eq!(
        contains_ignore_ascii_case(b"Hello World", b"WORLD"),
        Some(6)
    );
    assert_eq!(contains_ignore_ascii_case(b"HELLO", b"hello"), Some(0));
}

#[test]
fn a_needle_that_is_absent_or_too_long_finds_nothing() {
    assert_eq!(contains_ignore_ascii_case(b"hello", b"goodbye"), None);
    // Longer than the haystack: the early return, not a windows() panic.
    assert_eq!(contains_ignore_ascii_case(b"hi", b"hello there"), None);
    assert_eq!(contains_ignore_ascii_case(b"", b"x"), None);
}

/// Matches `str::find`, so a caller that passes an empty query gets "everything
/// matches" rather than "nothing does".
#[test]
fn an_empty_needle_matches_at_the_start() {
    assert_eq!(contains_ignore_ascii_case(b"anything", b""), Some(0));
    assert_eq!(contains_ignore_ascii_case(b"", b""), Some(0));
}

/// The documented limit. Pinned by a test so it is a decision rather than a
/// surprise: if someone adds folding later, this test is where they say so.
#[test]
fn non_ascii_case_is_not_folded() {
    assert_eq!(find_ignore_ascii_case("straße", "STRASSE"), None);
    assert_eq!(find_ignore_ascii_case("ÉCOLE", "école"), None);
    // ASCII either side of the multi-byte character still matches.
    assert_eq!(find_ignore_ascii_case("café LATTE", "latte"), Some(6));
}

#[test]
fn the_str_wrapper_finds_the_same_offsets() {
    assert_eq!(find_ignore_ascii_case("hello world", "WORLD"), Some(6));
    assert_eq!(find_ignore_ascii_case("hello", "nope"), None);
}

/// Scanning raw JSON means the needle meets escaped text. This is the limit the
/// API description has to carry, so it is pinned here too.
#[test]
fn a_needle_spanning_a_json_escape_does_not_match_the_escaped_form() {
    let raw_json = br#"{"content":"line one\nline two"}"#;
    // The logical text contains "one\nline" but the bytes contain a backslash-n.
    assert_eq!(contains_ignore_ascii_case(raw_json, b"one\nline"), None);
    // The escaped form is what is actually there.
    assert!(contains_ignore_ascii_case(raw_json, br"one\nline").is_some());
    // Text either side of the escape matches normally, which is why the
    // filter is still useful.
    assert!(contains_ignore_ascii_case(raw_json, b"line two").is_some());
}

#[test]
fn a_snippet_is_cut_around_the_match() {
    let text = format!("{}NEEDLE{}", "a".repeat(200), "b".repeat(200));
    let at = text.find("NEEDLE").unwrap();
    let out = snippet(&text, at);
    assert!(out.contains("NEEDLE"));
    assert!(out.starts_with('…'));
    assert!(out.ends_with('…'));
    // Bounded by the radius either side, plus the match and the two ellipses.
    assert!(out.chars().count() <= SNIPPET_RADIUS * 2 + "NEEDLE".len() + 2);
}

#[test]
fn a_short_text_snippets_whole_and_unmarked() {
    assert_eq!(snippet("short text", 0), "short text");
}
