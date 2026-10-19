//! A version bump has to be big enough for what it ships.
//!
//! Under semver a breaking change moves the major version, and below 1.0 it
//! moves the minor one instead: `0.6.4` to `0.7.0`, never to `0.6.5`. Clients
//! decide whether they can talk to a server by that number, so a patch bump
//! that ships a `**Breaking` changelog entry tells them a change is safe when
//! it is not, and the release that carries it is permanent.
//!
//! The entries a bump ships are the ones under `## Unreleased` before the
//! changelog is rolled, or under the new version's own heading after. Checking
//! either means `cargo xtask version` can refuse before it writes anything,
//! and CI can refuse a PR whose bump was written by hand.

use anyhow::Result;

/// The marker a breaking entry starts with, after its list bullet. It also
/// matches the qualified forms, such as `**Breaking, GraphQL.**`.
const BREAKING: &str = "**Breaking";

/// The changelog lines this bump ships: the `## Unreleased` section when there
/// is one, else the section headed by version `to`. `None` when neither exists.
pub fn shipped_section<'a>(changelog: &'a str, to: &str) -> Option<Vec<&'a str>> {
    let section_of = |wanted: &dyn Fn(&str) -> bool| {
        let mut lines = changelog.lines();
        lines.find(|line| line.strip_prefix("## ").is_some_and(wanted))?;
        Some(
            lines
                .take_while(|line| !line.starts_with("## "))
                .collect::<Vec<_>>(),
        )
    };
    section_of(&|heading| heading.trim_end() == "Unreleased")
        .or_else(|| section_of(&|heading| heading.split_whitespace().next() == Some(to)))
}

/// Whether any entry in `section` is marked breaking.
pub fn has_breaking_entry(section: &[&str]) -> bool {
    section.iter().any(|line| {
        line.trim_start()
            .strip_prefix(['-', '*'])
            .is_some_and(|entry| entry.trim_start().starts_with(BREAKING))
    })
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
/// the breaking level. Not moving the version at all is not a bump, so it
/// passes.
pub fn ensure_bump_level(changelog: &str, from: &str, to: &str) -> Result<()> {
    let breaking = from != to
        && shipped_section(changelog, to).is_some_and(|section| has_breaking_entry(&section))
        && !moves_breaking_level(from, to);
    anyhow::ensure!(
        !breaking,
        "{from} -> {to} ships a `**Breaking` changelog entry, so it has to move the {level} \
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
