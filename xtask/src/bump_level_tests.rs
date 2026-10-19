use super::*;

/// A changelog with one breaking entry under `## Unreleased`, the shape this
/// repo's changelog has while an incompatible change waits for its release.
const BREAKING_UNRELEASED: &str = "# Changelog\n\nIntro.\n\n## Unreleased\n\n### Changed\n\n\
     - **Breaking, GraphQL.** `spawnRun` takes a typed request.\n\
     - Something else.\n\n## 0.6.4 - 2026-09-26\n\n- **Breaking.** An old one.\n";

/// The same changelog after `cargo xtask version 0.6.5` rolled it.
const BREAKING_ROLLED: &str = "# Changelog\n\n## 0.6.5 - 2026-10-18\n\n\
     - **Breaking.** A typed request.\n\n## 0.6.4 - 2026-09-26\n\n- Fixed a thing.\n";

/// Only additive and fixed entries are unreleased; the breaking one shipped.
const ADDITIVE_UNRELEASED: &str = "# Changelog\n\n## Unreleased\n\n### Added\n\n\
     - A capability.\n\n## 0.6.4 - 2026-09-26\n\n- **Breaking.** An old one.\n";

#[test]
fn a_patch_bump_with_a_breaking_entry_is_refused() {
    let err = ensure_bump_level(BREAKING_UNRELEASED, "0.6.4", "0.6.5")
        .unwrap_err()
        .to_string();
    assert!(err.contains("0.6.4 -> 0.6.5"), "{err}");
    assert!(err.contains("minor"), "{err}");
    assert!(err.contains("0.7.0"), "{err}");
}

#[test]
fn a_minor_bump_with_a_breaking_entry_passes_below_one() {
    ensure_bump_level(BREAKING_UNRELEASED, "0.6.4", "0.7.0").unwrap();
    ensure_bump_level(BREAKING_UNRELEASED, "0.6.4", "1.0.0").unwrap();
}

#[test]
fn from_one_up_only_the_major_version_carries_a_breaking_change() {
    let err = ensure_bump_level(BREAKING_UNRELEASED, "1.2.3", "1.3.0")
        .unwrap_err()
        .to_string();
    assert!(err.contains("major"), "{err}");
    assert!(err.contains("2.0.0"), "{err}");
    ensure_bump_level(BREAKING_UNRELEASED, "1.2.3", "2.0.0").unwrap();
}

#[test]
fn a_rolled_changelog_is_read_under_the_new_version_heading() {
    assert!(ensure_bump_level(BREAKING_ROLLED, "0.6.4", "0.6.5").is_err());
    ensure_bump_level(BREAKING_ROLLED, "0.6.4", "0.7.0").unwrap();
}

#[test]
fn a_patch_bump_without_a_breaking_entry_passes() {
    ensure_bump_level(ADDITIVE_UNRELEASED, "0.6.4", "0.6.5").unwrap();
}

#[test]
fn an_unmoved_version_is_not_a_bump() {
    ensure_bump_level(BREAKING_UNRELEASED, "0.6.4", "0.6.4").unwrap();
}

#[test]
fn a_changelog_with_no_shipped_section_has_nothing_to_refuse() {
    ensure_bump_level(
        "# Changelog\n\n## 0.6.4 - x\n\n- **Breaking.** old\n",
        "0.6.4",
        "0.6.5",
    )
    .unwrap();
    assert_eq!(shipped_section("# Changelog\n", "0.6.5"), None);
}

#[test]
fn the_shipped_section_stops_at_the_next_heading() {
    let section = shipped_section(BREAKING_UNRELEASED, "0.6.5").unwrap();
    assert!(section.iter().any(|line| line.contains("typed request")));
    assert!(!section.iter().any(|line| line.contains("An old one")));
    let empty = shipped_section("## Unreleased\n## 0.6.4\n- x\n", "0.6.5").unwrap();
    assert!(empty.is_empty(), "{empty:?}");
}

#[test]
fn only_a_bulleted_entry_counts_as_breaking() {
    assert!(has_breaking_entry(&["- **Breaking.** x"]));
    assert!(has_breaking_entry(&["  * **Breaking, library.** x"]));
    assert!(!has_breaking_entry(&["Prose about **Breaking** changes."]));
    assert!(!has_breaking_entry(&["- Not **Breaking** at the start."]));
}

#[test]
fn the_breaking_level_follows_semver() {
    assert!(moves_breaking_level("0.6.4", "0.7.0"));
    assert!(moves_breaking_level("0.6.4", "1.0.0"));
    assert!(!moves_breaking_level("0.6.4", "0.6.10"));
    assert!(moves_breaking_level("1.9.9", "2.0.0"));
    assert!(!moves_breaking_level("1.9.9", "1.10.0"));
    assert!(!moves_breaking_level("0.7.0", "0.6.9"));
    assert_eq!(numbers("0.7"), [0, 7, 0]);
    assert_eq!(numbers("x.7.1"), [0, 7, 1]);
}
