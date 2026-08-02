//! `cargo xtask version` - move the workspace version, or check it is coherent.
//!
//! A release is triggered by the workspace version moving, which makes the bump
//! the single most consequential edit in the repo - and it is spread across
//! twelve lines of the root manifest. `[workspace.package] version` is what
//! every crate inherits, and the eleven `[workspace.dependencies]` entries each
//! repeat it because `cargo publish` refuses a path dependency with no version
//! requirement. Cargo offers no way to make those requirements inherit the
//! workspace version, so they have to be written out, and they have to agree.
//!
//! Getting that wrong is quiet. Within a `0.1.x` line a stale `version =
//! "0.1.2"` pin is a `^0.1.2` requirement that `0.1.3` satisfies, so everything
//! builds and publishes - it just ships crates whose interdependencies name the
//! previous version. It only turns loud on a bump that crosses the caret
//! boundary (`0.1.x` to `0.2.0`), and by then the alpha shipped a week earlier.
//!
//! So: `set` writes all twelve at once, and `check` is the CI guard that fails
//! a hand-edit which touched only some of them.

use anyhow::{Context, Result};

use crate::coverage::Runner;

/// Path of the manifest holding every version declaration, relative to the
/// workspace root.
const MANIFEST: &str = "Cargo.toml";

/// Path of the changelog whose `## Unreleased` heading a bump rolls over.
const CHANGELOG: &str = "CHANGELOG.md";

// ── CLI argument parsing ─────────────────────────────────────────────────────

/// What `cargo xtask version` was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum VersionMode {
    /// Rewrite every declaration to this version and roll the changelog.
    Set(String),
    /// Verify the declarations already agree. Changes nothing; CI runs this.
    Check,
}

impl VersionMode {
    /// Parse the arguments following `cargo xtask version`.
    pub fn parse(args: &[String]) -> Result<Self> {
        match args {
            [] => anyhow::bail!("Usage: cargo xtask version <X.Y.Z> | cargo xtask version --check"),
            [flag] if flag == "--check" => Ok(Self::Check),
            [candidate] => {
                validate_semver(candidate)?;
                Ok(Self::Set(candidate.clone()))
            }
            _ => anyhow::bail!("cargo xtask version takes exactly one argument"),
        }
    }
}

/// Reject anything that is not a bare `X.Y.Z`.
///
/// Deliberately stricter than semver proper: the release workflows validate the
/// version they read out of this manifest against the same shape before they
/// tag with it, so a prerelease or build-metadata suffix written here would
/// only fail later, in a release run.
pub fn validate_semver(candidate: &str) -> Result<()> {
    let parts: Vec<&str> = candidate.split('.').collect();
    let well_formed = parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    anyhow::ensure!(
        well_formed,
        "'{candidate}' is not a bare X.Y.Z version (the release workflows reject anything else)"
    );
    Ok(())
}

// ── Reading what the manifest declares ───────────────────────────────────────

/// The version `[workspace.package]` declares, which every crate inherits.
///
/// Found by the same line-anchored match the release workflows use, so what
/// this reports and what a release tags with cannot diverge: only the
/// workspace version sits at the start of a line, since a dependency entry
/// always begins with the crate name.
pub fn workspace_version(manifest: &str) -> Result<String> {
    manifest
        .lines()
        .find_map(|line| line.strip_prefix("version = ").and_then(unquote))
        .context("no `version = \"...\"` line in the root Cargo.toml")
}

/// Every intra-workspace path dependency that pins a version, as
/// `(crate name, pinned version)`, in manifest order.
///
/// `leviath-testkit` is absent by design rather than by omission: it is
/// path-only so cargo strips it when publishing, which is what keeps the
/// testkit private to the repo. Having no version to pin, it has none to check.
pub fn pinned_versions(manifest: &str) -> Vec<(String, String)> {
    manifest
        .lines()
        .filter(|line| line.contains("path = \"crates/"))
        .filter_map(|line| {
            let name = line.split(" = ").next()?.trim();
            let pinned = pin_of(line)?;
            Some((name.to_owned(), pinned))
        })
        .collect()
}

/// The version a dependency line pins, if it pins one.
fn pin_of(line: &str) -> Option<String> {
    let (_, after) = line.split_once("version = ")?;
    unquote(after)
}

/// The contents of the leading `"..."` of `text`.
///
/// Split rather than indexed: the workspace denies `clippy::string_slice`,
/// because a byte range that lands mid-character panics.
fn unquote(text: &str) -> Option<String> {
    let rest = text.strip_prefix('"')?;
    let (inner, _) = rest.split_once('"')?;
    Some(inner.to_owned())
}

// ── Check ────────────────────────────────────────────────────────────────────

/// Names of the dependencies whose pin disagrees with the workspace version.
///
/// Returned rather than reported so the caller owns the message and this stays
/// a pure function over the manifest text.
pub fn disagreeing_pins(manifest: &str) -> Result<Vec<String>> {
    let expected = workspace_version(manifest)?;
    Ok(pinned_versions(manifest)
        .into_iter()
        .filter(|(_, pinned)| *pinned != expected)
        .map(|(name, pinned)| format!("{name} pins {pinned}"))
        .collect())
}

// ── Set ──────────────────────────────────────────────────────────────────────

/// Rewrite the workspace version and every intra-workspace pin to `new`.
///
/// Line-by-line rather than through a TOML round-trip on purpose: reserialising
/// would reflow the comments that explain why each of these lines exists, and
/// those comments are the only thing standing between a future reader and
/// deleting the pins as redundant.
pub fn set_versions(manifest: &str, new: &str) -> Result<String> {
    validate_semver(new)?;
    let mut rewrote_workspace = false;
    let mut out: Vec<String> = Vec::with_capacity(manifest.lines().count());

    for line in manifest.lines() {
        if line.starts_with("version = ") && !rewrote_workspace {
            rewrote_workspace = true;
            out.push(format!("version = \"{new}\""));
        } else if line.contains("path = \"crates/") && pin_of(line).is_some() {
            out.push(replace_pin(line, new));
        } else {
            out.push(line.to_owned());
        }
    }

    anyhow::ensure!(
        rewrote_workspace,
        "no `version = \"...\"` line in the root Cargo.toml"
    );
    let mut text = out.join("\n");
    if manifest.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

/// Swap the version a dependency line pins, leaving the rest of the line -
/// path, features, trailing comment - exactly as it was.
fn replace_pin(line: &str, new: &str) -> String {
    let Some((before, after)) = line.split_once("version = \"") else {
        return line.to_owned();
    };
    let Some((_, tail)) = after.split_once('"') else {
        return line.to_owned();
    };
    format!("{before}version = \"{new}\"{tail}")
}

/// Roll `## Unreleased` under a dated heading and open a fresh empty one.
///
/// The entries themselves are left untouched: what was accumulating under
/// `## Unreleased` is exactly what this version ships.
pub fn roll_changelog(changelog: &str, version: &str, date: &str) -> Result<String> {
    anyhow::ensure!(
        changelog.contains("\n## Unreleased\n"),
        "no `## Unreleased` heading in {CHANGELOG} - add one before bumping"
    );
    Ok(changelog.replacen(
        "\n## Unreleased\n",
        &format!("\n## Unreleased\n\n## {version} - {date}\n"),
        1,
    ))
}

// ── Dates ────────────────────────────────────────────────────────────────────

/// Today in UTC as `YYYY-MM-DD`.
pub fn today() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0) as i64;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The civil date `(year, month, day)` for a day number counted from the UNIX
/// epoch, via Howard Hinnant's `civil_from_days`.
///
/// Arithmetic rather than a `date` subprocess or a calendar dependency, so the
/// changelog heading is testable without a clock.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

// ── Entry points ─────────────────────────────────────────────────────────────

/// Real entry point: reads and writes the workspace files.
pub fn run(mode: VersionMode) -> Result<()> {
    run_with(&crate::coverage::RealRunner, mode)
}

/// Entry point with the subprocess runner injected, so tests can drive the
/// `cargo update` step without spawning cargo.
pub fn run_with(runner: &dyn Runner, mode: VersionMode) -> Result<()> {
    let manifest = std::fs::read_to_string(MANIFEST)
        .with_context(|| format!("reading {MANIFEST} - run this from the workspace root"))?;

    match mode {
        VersionMode::Check => {
            let disagreeing = disagreeing_pins(&manifest)?;
            if disagreeing.is_empty() {
                println!(
                    "Every intra-workspace pin matches [workspace.package] version {}.",
                    workspace_version(&manifest)?
                );
                return Ok(());
            }
            anyhow::bail!(
                "[workspace.package] version is {}, but {}. \
                 Run `cargo xtask version <X.Y.Z>` rather than editing them by hand.",
                workspace_version(&manifest)?,
                disagreeing.join(", ")
            )
        }
        VersionMode::Set(new) => {
            let previous = workspace_version(&manifest)?;
            std::fs::write(MANIFEST, set_versions(&manifest, &new)?)
                .with_context(|| format!("writing {MANIFEST}"))?;

            let changelog = std::fs::read_to_string(CHANGELOG)
                .with_context(|| format!("reading {CHANGELOG}"))?;
            std::fs::write(CHANGELOG, roll_changelog(&changelog, &new, &today())?)
                .with_context(|| format!("writing {CHANGELOG}"))?;

            // The lockfile records the workspace crates' own versions, and CI
            // rejects a lockfile that disagrees with the manifests.
            anyhow::ensure!(
                runner.cargo(&["update", "--workspace"])?,
                "cargo update --workspace failed; {MANIFEST} and {CHANGELOG} were still written"
            );

            println!("Version {previous} -> {new}.");
            println!("Review the changes, then merging them to main cuts the alpha release.");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest shaped like the real root one: the workspace version at the
    /// start of a line, intra-workspace pins, a path-only member, and
    /// third-party entries that must never be rewritten.
    const MANIFEST: &str = concat!(
        "[workspace.package]\n",
        "version = \"0.1.2\"\n",
        "edition = \"2024\"\n",
        "\n",
        "[workspace.dependencies]\n",
        "leviath = { path = \"crates/leviath\", version = \"0.1.2\" }\n",
        "leviath-core = { path = \"crates/leviath-core\", version = \"0.1.2\" }\n",
        "leviath-testkit = { path = \"crates/leviath-testkit\" }\n",
        "serde = { version = \"1.0\", features = [\"derive\"] }\n",
    );

    // ── Argument parsing ──────────────────────────────────────────────────────

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parse_no_args_explains_both_forms() {
        let err = VersionMode::parse(&args(&[])).unwrap_err().to_string();
        assert!(
            err.contains("--check"),
            "usage should mention --check: {err}"
        );
    }

    #[test]
    fn parse_check_flag_selects_check() {
        assert_eq!(
            VersionMode::parse(&args(&["--check"])).unwrap(),
            VersionMode::Check
        );
    }

    #[test]
    fn parse_version_selects_set() {
        assert_eq!(
            VersionMode::parse(&args(&["1.2.3"])).unwrap(),
            VersionMode::Set("1.2.3".to_owned())
        );
    }

    #[test]
    fn parse_rejects_extra_arguments() {
        assert!(VersionMode::parse(&args(&["1.2.3", "4.5.6"])).is_err());
    }

    // ── Semver validation ─────────────────────────────────────────────────────

    #[test]
    fn validate_accepts_a_bare_triple() {
        assert!(validate_semver("0.1.2").is_ok());
        assert!(validate_semver("10.20.30").is_ok());
    }

    #[test]
    fn validate_rejects_a_prerelease_suffix() {
        // The release workflows tag from this value and reject anything that is
        // not a bare triple, so accepting one here would only fail later.
        assert!(validate_semver("0.1.2-alpha").is_err());
    }

    #[test]
    fn validate_rejects_wrong_component_counts() {
        assert!(validate_semver("0.1").is_err());
        assert!(validate_semver("0.1.2.3").is_err());
    }

    #[test]
    fn validate_rejects_empty_and_non_numeric_components() {
        assert!(validate_semver("0..2").is_err());
        assert!(validate_semver("0.1.x").is_err());
    }

    // ── Reading declarations ──────────────────────────────────────────────────

    #[test]
    fn workspace_version_reads_the_line_anchored_entry() {
        assert_eq!(workspace_version(MANIFEST).unwrap(), "0.1.2");
    }

    #[test]
    fn workspace_version_errors_when_absent() {
        assert!(workspace_version("[workspace]\nmembers = []\n").is_err());
    }

    #[test]
    fn pinned_versions_finds_intra_workspace_pins_only() {
        let pins = pinned_versions(MANIFEST);
        assert_eq!(
            pins,
            vec![
                ("leviath".to_owned(), "0.1.2".to_owned()),
                ("leviath-core".to_owned(), "0.1.2".to_owned()),
            ],
            "path-only members and third-party crates must be left out"
        );
    }

    // ── Check ─────────────────────────────────────────────────────────────────

    #[test]
    fn disagreeing_pins_is_empty_when_everything_matches() {
        assert!(disagreeing_pins(MANIFEST).unwrap().is_empty());
    }

    #[test]
    fn disagreeing_pins_names_the_stale_entry() {
        let stale = MANIFEST.replace(
            "leviath-core\", version = \"0.1.2\"",
            "leviath-core\", version = \"0.1.1\"",
        );
        let found = disagreeing_pins(&stale).unwrap();
        assert_eq!(found, vec!["leviath-core pins 0.1.1".to_owned()]);
    }

    #[test]
    fn disagreeing_pins_propagates_a_missing_workspace_version() {
        assert!(
            disagreeing_pins("leviath = { path = \"crates/leviath\", version = \"1.0.0\" }\n")
                .is_err()
        );
    }

    // ── Set ───────────────────────────────────────────────────────────────────

    #[test]
    fn set_versions_rewrites_the_workspace_version_and_every_pin() {
        let out = set_versions(MANIFEST, "0.2.0").unwrap();
        assert_eq!(workspace_version(&out).unwrap(), "0.2.0");
        assert!(disagreeing_pins(&out).unwrap().is_empty());
    }

    #[test]
    fn set_versions_leaves_third_party_pins_alone() {
        let out = set_versions(MANIFEST, "0.2.0").unwrap();
        assert!(
            out.contains("serde = { version = \"1.0\", features = [\"derive\"] }"),
            "a third-party requirement must survive verbatim:\n{out}"
        );
        assert!(out.contains("leviath-testkit = { path = \"crates/leviath-testkit\" }"));
    }

    #[test]
    fn set_versions_preserves_the_trailing_newline() {
        assert!(set_versions(MANIFEST, "0.2.0").unwrap().ends_with('\n'));
        assert!(
            !set_versions("version = \"0.1.2\"", "0.2.0")
                .unwrap()
                .ends_with('\n')
        );
    }

    #[test]
    fn set_versions_rejects_a_malformed_version() {
        assert!(set_versions(MANIFEST, "nope").is_err());
    }

    #[test]
    fn set_versions_errors_when_there_is_no_workspace_version() {
        assert!(set_versions("[workspace]\nmembers = []\n", "0.2.0").is_err());
    }

    #[test]
    fn set_versions_rewrites_only_the_first_anchored_version() {
        // `[package] version` further down a manifest must not be mistaken for
        // the workspace one.
        let two = "version = \"0.1.2\"\n[other]\nversion = \"9.9.9\"\n";
        let out = set_versions(two, "0.2.0").unwrap();
        assert!(out.starts_with("version = \"0.2.0\""));
        assert!(
            out.contains("version = \"9.9.9\""),
            "second entry untouched:\n{out}"
        );
    }

    #[test]
    fn replace_pin_returns_lines_it_cannot_parse_unchanged() {
        assert_eq!(replace_pin("no version here", "1.0.0"), "no version here");
        assert_eq!(
            replace_pin("version = \"unterminated", "1.0.0"),
            "version = \"unterminated"
        );
    }

    #[test]
    fn unquote_rejects_text_that_is_not_a_quoted_string() {
        assert_eq!(unquote("bare"), None);
        assert_eq!(unquote("\"unterminated"), None);
    }

    // ── Changelog ─────────────────────────────────────────────────────────────

    #[test]
    fn roll_changelog_inserts_a_dated_heading_below_a_fresh_unreleased() {
        let rolled = roll_changelog(
            "# Changelog\n\n## Unreleased\n\n- did a thing\n\n## 0.1.1 - 2026-07-31\n",
            "0.1.2",
            "2026-08-02",
        )
        .unwrap();
        assert!(rolled.contains("## Unreleased\n\n## 0.1.2 - 2026-08-02\n\n- did a thing"));
        assert!(rolled.contains("## 0.1.1 - 2026-07-31"), "history is kept");
    }

    #[test]
    fn roll_changelog_rolls_only_the_first_unreleased() {
        let rolled =
            roll_changelog("x\n## Unreleased\n## Unreleased\n", "1.0.0", "2026-01-01").unwrap();
        assert_eq!(rolled.matches("## 1.0.0 - 2026-01-01").count(), 1);
    }

    #[test]
    fn roll_changelog_errors_without_an_unreleased_heading() {
        assert!(
            roll_changelog(
                "# Changelog\n\n## 0.1.1 - 2026-07-31\n",
                "0.1.2",
                "2026-08-02"
            )
            .is_err()
        );
    }

    // ── Dates ─────────────────────────────────────────────────────────────────

    #[test]
    fn civil_from_days_converts_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        // 2024 is a leap year: day 60 of it is 29 February.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(20_667), (2026, 8, 2));
    }

    #[test]
    fn civil_from_days_handles_dates_before_the_epoch() {
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(-719_468), (0, 3, 1));
    }

    #[test]
    fn today_is_a_zero_padded_iso_date() {
        let today = today();
        assert_eq!(today.len(), 10, "expected YYYY-MM-DD, got {today}");
        let parts: Vec<&str> = today.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit())));
    }
}
