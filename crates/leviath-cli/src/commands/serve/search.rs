//! Matching primitives for server-side search.
//!
//! Search over runs is deliberately two-phase, and these are the pieces phase
//! one is built from.
//!
//! **Phase one is a candidate filter that never parses anything.** It answers
//! "could this run match" over raw bytes: the already-parsed `RunMeta` for the
//! cheap sources, and unparsed file contents for the deep ones. It must stay
//! cheap because it runs over *every* candidate run, and the run set grows
//! without bound - nothing prunes it.
//!
//! **Phase two builds the highlights**, and runs only over the items actually
//! being returned. That is where parsing is affordable, because it is bounded by
//! the page size rather than by how long the daemon has been installed.
//!
//! The measured reason this works without an index: a substring scan across
//! every journal on a real machine - 84 archives, 22 MB - takes about 10 ms
//! warm. What is *not* affordable is replaying those journals, which deep-copies
//! a whole context window per recorded point.

/// Find `needle` in `haystack`, ignoring ASCII case, returning the byte offset.
///
/// Returns the offset rather than a bool so phase two can reuse the position to
/// cut a snippet instead of searching again.
///
/// Two deliberate limits, both of which belong in the API description rather
/// than being discovered by a user:
///
/// - **ASCII case only.** No Unicode case folding, so `Straße` does not match
///   `STRASSE`. Doing this properly means allocating a case-folded copy of every
///   haystack, and the haystacks here are megabyte-scale files.
/// - It works on raw bytes, so when the haystack is JSON (a context snapshot or
///   a run journal) the needle is matched against the *escaped* text. A query
///   containing a quote, a backslash, a newline, or a character serde escapes
///   may not match even though the underlying text contains it.
///
/// An empty needle matches at 0, which is the same convention `str::find` uses.
pub(super) fn contains_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

/// [`contains_ignore_ascii_case`] over `&str`, for the in-memory metadata
/// sources where the haystack is already text.
pub(super) fn find_ignore_ascii_case(haystack: &str, needle: &str) -> Option<usize> {
    contains_ignore_ascii_case(haystack.as_bytes(), needle.as_bytes())
}

/// How much text to show either side of a match.
pub(super) const SNIPPET_RADIUS: usize = 80;

/// Build the snippet for a match at `at` in `text`.
pub(super) fn snippet(text: &str, at: usize) -> String {
    leviath_core::text::snippet_around(text, at, SNIPPET_RADIUS)
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
