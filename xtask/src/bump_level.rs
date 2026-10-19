//! A version bump has to be big enough for what it ships.
//!
//! Under semver a breaking change moves the major version, and below 1.0 it
//! moves the minor one instead: `0.6.4` to `0.7.0`, never to `0.6.5`. Clients
//! decide whether they can talk to a server by that number, so a patch bump
//! that ships a breaking changelog entry tells them a change is safe when it
//! is not, and the release that carries it is permanent.
//!
//! The entries a bump ships are the ones under `## Unreleased` before the
//! changelog is rolled, and under the new version's own heading after. Both
//! are read, whichever exist, so `cargo xtask version` can refuse before it
//! writes anything, and CI can refuse a PR whose bump was written by hand,
//! even one that opened a fresh `## Unreleased` above the rolled section.

use anyhow::Result;

/// The word that marks a breaking entry or a heading over breaking entries.
const BREAKING: &str = "breaking";

/// The changelog lines this bump ships: the `## Unreleased` section and the
/// section headed by version `to`, whichever exist. Either heading may be
/// bracketed, as in `## [Unreleased]` or `## [0.7.0] - 2026-10-18`.
pub fn shipped_lines<'a>(changelog: &'a str, to: &str) -> Vec<&'a str> {
    let mut shipping = false;
    changelog
        .lines()
        .filter(|line| {
            if let Some(heading) = line.strip_prefix("## ") {
                let name = heading.split_whitespace().next().unwrap_or_default();
                let name = name.trim_start_matches('[').trim_end_matches(']');
                shipping = name.eq_ignore_ascii_case("unreleased") || name == to;
                return false;
            }
            shipping
        })
        .collect()
}

/// Whether `line` is a breaking entry or a heading over breaking entries:
/// `### Breaking`, or a `-`, `*`, `+` or numbered bullet that starts with the
/// word, bold or not, as in `- **Breaking, GraphQL.**` or `1. Breaking: x`.
fn is_breaking(line: &str) -> bool {
    entry_text(line.trim_start()).is_some_and(|text| {
        let text = text.trim_start().trim_start_matches(['*', '_']);
        let (word, rest) = text.split_at_checked(BREAKING.len()).unwrap_or(("", ""));
        word.eq_ignore_ascii_case(BREAKING) && !rest.starts_with(|c: char| c.is_alphanumeric())
    })
}

/// The text of a heading or list item after its marker, or `None` for a line
/// that is neither.
fn entry_text(line: &str) -> Option<&str> {
    if line.starts_with('#') {
        return Some(line.trim_start_matches('#'));
    }
    if let Some(entry) = line.strip_prefix(['-', '*', '+']) {
        return Some(entry);
    }
    let rest = line.trim_start_matches(|c: char| c.is_ascii_digit());
    match rest.len() < line.len() {
        true => rest.strip_prefix(['.', ')']),
        false => None,
    }
}

/// Whether any line in `section` is a breaking entry or heading.
pub fn has_breaking_entry(section: &[&str]) -> bool {
    section.iter().any(|line| is_breaking(line))
}

/// The major, minor and patch numbers of a bare `X.Y.Z`. A part that is
/// missing or not a number counts as 0.
fn numbers(version: &str) -> [u64; 3] {
    let mut parts = version.split('.').map(|part| part.parse().unwrap_or(0));
    [(); 3].map(|()| parts.next().unwrap_or(0))
}

/// Whether going from `from` to `to` moves the number semver reserves for a
/// breaking change: the major version, or the minor one while the major is 0.
pub fn moves_breaking_level(from: &str, to: &str) -> bool {
    let [from_major, from_minor, _] = numbers(from);
    let [to_major, to_minor, _] = numbers(to);
    match from_major {
        0 => to_major > 0 || to_minor > from_minor,
        _ => to_major > from_major,
    }
}

/// Refuse a bump from `from` to `to` that ships a breaking entry without moving
/// the breaking level. Only a forward bump is checked: an unmoved version is no
/// bump at all, and a version that goes backwards is a revert, which has no
/// level to be too small for.
pub fn ensure_bump_level(changelog: &str, from: &str, to: &str) -> Result<()> {
    let forward = numbers(to) > numbers(from);
    let breaking = forward
        && has_breaking_entry(&shipped_lines(changelog, to))
        && !moves_breaking_level(from, to);
    anyhow::ensure!(
        !breaking,
        "{from} -> {to} ships a breaking changelog entry, so it has to move the {level} \
         version, not just the patch: semver reserves that number for breaking changes, \
         and clients rely on it to tell whether they still fit. Bump to {suggested} instead.",
        level = if numbers(from)[0] == 0 {
            "minor"
        } else {
            "major"
        },
        suggested = suggested(from),
    );
    Ok(())
}

/// The smallest version after `from` that may carry a breaking change.
fn suggested(from: &str) -> String {
    match numbers(from) {
        [0, minor, _] => format!("0.{}.0", minor + 1),
        [major, _, _] => format!("{}.0.0", major + 1),
    }
}

#[cfg(test)]
#[path = "bump_level_tests.rs"]
mod tests;
