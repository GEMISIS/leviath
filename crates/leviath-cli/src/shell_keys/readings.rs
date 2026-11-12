//! The paths a program might open when it is handed one argument.
//!
//! The read fence asks every reading whether it stays inside the working
//! directory, and a single one that does not makes the word count as
//! outside. So a reading listed here costs a prompt at worst, and one left
//! out is a read nobody checked.
//!
//! A value is read whole, every way [`spellings`] lists, and in [`parts`]:
//! what stands between its `:`, `,` and `=`, because a program may take a
//! word apart there. `file -m a:/etc/hosts` opens both files of its list, and
//! `dd if=/etc/hosts` the one after the `=`. A word is never inside on the
//! strength of how it reads whole.
//!
//! Two things take whole readings away, and each is bounded by what a program
//! could really do with the word:
//!
//! - **`-name=value`.** A single dash, a name, an `=`: the value is what
//!   follows the `=`, as it is for `--name=value`. The readings that begin
//!   inside the name are kept, because a flag that takes its value glued on
//!   (`-fa=b` as `-f` with `a=b`) opens a file called `a=b`. The ones that
//!   begin inside the value are not: no flag is spelled `-name=va`.
//! - **A URL.** `scheme://rest` names something by a scheme, so the `//`
//!   after the colon is not the start of an absolute path. It is still read
//!   two ways. As it is spelled, which is how a program that knows nothing of
//!   URLs opens it. And as `rest` alone, which is what a program that strips
//!   the scheme opens: `x:///etc/passwd` is `/etc/passwd` to it, and
//!   `x://../../etc/passwd` climbs. A `file:` URL is a path outright, and its
//!   path is read as one. The part after the colon, `//rest`, is still one of
//!   the value's parts, for the program that splits on the colon.

/// One way a value may be read as a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Reading<'a> {
    /// The path.
    pub(super) text: &'a str,
    /// Whether it is one of the value's [`parts`] rather than the value
    /// whole. A program opens a part only if it takes the value apart, so an
    /// absolute part with nothing at the root to open is no read at all.
    pub(super) part: bool,
}

/// Every way a program could read `value` as a path: whole, as [`spellings`]
/// lists, and in [`parts`]. `None` when one of them cannot be placed at all.
pub(super) fn readings(value: &str) -> Option<Vec<Reading<'_>>> {
    let whole = spellings(value)?
        .into_iter()
        .map(|text| Reading { text, part: false });
    let parts = parts(value)
        .into_iter()
        .map(|text| Reading { text, part: true });
    Some(whole.chain(parts).collect())
}

/// Where a program may take a word apart.
const SEPARATORS: [char; 3] = [':', ',', '='];

/// The parts of `value`: what stands between its `:`, `,` and `=`. None for
/// a value that has no such separator, which has no reading but the whole.
pub(super) fn parts(value: &str) -> Vec<&str> {
    match value.contains(SEPARATORS) {
        true => value
            .split(SEPARATORS)
            .filter(|part| !part.is_empty())
            .collect(),
        false => Vec::new(),
    }
}

/// The paths a program could open when it is handed `value` whole, as a
/// plain word or as an option's value. `None` when one of them cannot be
/// placed at all: a `file:` URL on another host, or one that hides its path
/// behind percent-encoding.
pub(super) fn spellings(value: &str) -> Option<Vec<&str>> {
    let mut out = vec![value];
    // What is left once each leading `scheme://` is taken off.
    let mut rest = value;
    while let Some(after) = after_scheme(rest) {
        out.push(after);
        rest = after;
    }
    // A `file:` URL names a path wherever in the word it starts: `jar:file:`,
    // `git+file://`.
    let lower = value.to_ascii_lowercase();
    for (at, _) in lower.match_indices("file:") {
        let before = lower.get(..at).and_then(|b| b.chars().next_back());
        if before.is_some_and(|c| c.is_ascii_alphanumeric()) {
            continue;
        }
        // ASCII lowering keeps every byte offset, so `at` is one in `value`.
        let after = value.get(at + "file:".len()..).unwrap_or_default();
        out.push(file_path(after)?);
    }
    Some(out)
}

/// What follows the `scheme://` that `value` starts with, when it starts
/// with one. A scheme is spelled as RFC 3986 spells it: a letter, then
/// letters, digits, `+`, `-` and `.`.
fn after_scheme(value: &str) -> Option<&str> {
    let (scheme, rest) = value.split_once("://")?;
    is_scheme(scheme).then_some(rest)
}

/// Whether `text` is spelled as a URL scheme.
fn is_scheme(text: &str) -> bool {
    text.starts_with(|c: char| c.is_ascii_alphabetic()) && text.chars().all(is_scheme_char)
}

/// Whether `c` may appear in a URL scheme.
fn is_scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')
}

/// The path a `file:` URL names, from what follows `file:`. `None` when it
/// names another host, or percent-encodes anything: `%2e%2e` is `..` to the
/// program that decodes it and a plain name to a fence that does not.
fn file_path(after: &str) -> Option<&str> {
    let path = match after.strip_prefix("//") {
        Some(rest) => {
            let (host, path) = match rest.find('/') {
                Some(slash) => rest.split_at(slash),
                None => (rest, "/"),
            };
            (host.is_empty() || host.eq_ignore_ascii_case("localhost")).then_some(path)?
        }
        None => after,
    };
    (!path.contains('%')).then_some(path)
}

/// The values one argument could carry, each of which [`spellings`] reads.
///
/// A plain word is one. A `--flag=value` is its value, and a long flag with no
/// value is read as a path too, which costs nothing when it is not one. A
/// `-name=value` is its value, and each reading that starts inside the name.
/// Any other short flag could have a value glued on after any letter
/// (`-f../list`, `-rf/etc/x`), so every tail is one; a harmless flag read as
/// an absolute path costs a prompt. The one tail that is not read is the
/// inside of a URL glued to a flag (`-ohttp://host/x`), where the tails after
/// the colon would be `//host/x` and `/x`: what follows the `://` is read in
/// their place.
pub(super) fn candidates(text: &str) -> Vec<&str> {
    let Some(body) = text.strip_prefix('-').filter(|body| !body.is_empty()) else {
        return vec![text];
    };
    if let Some(long) = body.strip_prefix('-') {
        return vec![long.split_once('=').map_or(text, |(_, value)| value)];
    }
    // Where the readings that are tails of the word stop, and the one value
    // read past that point.
    let (last_tail, value) = match body.split_once('=') {
        Some((name, value)) if is_flag_name(name) => (name.len(), value),
        _ => match glued_url(body) {
            Some((colon, rest)) => (colon.saturating_sub(1), rest),
            None => (body.len().saturating_sub(1), ""),
        },
    };
    // A tail starts after the first letter at the earliest: the flag itself.
    let mut out: Vec<&str> = (1..=last_tail).filter_map(|at| body.get(at..)).collect();
    if !value.is_empty() {
        out.push(value);
    }
    out
}

/// Whether `name` is spelled as the name of a flag that takes `=value`: a
/// letter, then letters, digits, `-` and `_`.
fn is_flag_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// A URL glued into a short flag's `body`: where its colon is, and what
/// follows its `://`. `None` when no `://` in the body has a scheme in front
/// of it.
fn glued_url(body: &str) -> Option<(usize, &str)> {
    let (before, rest) = body.split_once("://")?;
    // The scheme is the longest run of scheme characters before the colon
    // that starts with a letter.
    let run = before
        .rfind(|c: char| !is_scheme_char(c))
        .map_or(before, |at| before.get(at + 1..).unwrap_or_default());
    run.contains(|c: char| c.is_ascii_alphabetic())
        .then_some((before.len(), rest))
}

#[cfg(test)]
#[path = "readings_tests.rs"]
mod tests;
