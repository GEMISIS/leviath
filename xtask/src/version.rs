//! `cargo xtask version` - move the workspace version, or check it is coherent.
//!
//! A release is triggered by the workspace version moving, which makes the bump
//! the single most consequential edit in the repo - and it is spread across
//! thirteen lines of two manifests. `[workspace.package] version` is what every
//! crate inherits, and the eleven `[workspace.dependencies]` entries each repeat
//! it because `cargo publish` refuses a path dependency with no version
//! requirement. Cargo offers no way to make those requirements inherit the
//! workspace version, so they have to be written out, and they have to agree.
//! The thirteenth is `leviath-cli`'s allocator pin, which lives in that crate's
//! own manifest on purpose - only the composition-root binary should pick an
//! allocator - and so is invisible from the root.
//!
//! Getting that wrong is quiet. Within a `0.1.x` line a stale `version =
//! "0.1.2"` pin is a `^0.1.2` requirement that `0.1.3` satisfies, so everything
//! builds and publishes - it just ships crates whose interdependencies name the
//! previous version. It only turns loud on a bump that crosses the caret
//! boundary (`0.1.x` to `0.2.0`), and by then the alpha shipped a week earlier.
//!
//! So: `set` writes all thirteen at once, and `check` is the CI guard that
//! fails a hand-edit which touched only some of them.
//!
//! `check` also compares the two `[profile.release]` blocks, for the same
//! reason: `leviath-cli`'s copy is what a `cargo install leviath-cli` build
//! uses (that manifest is the root when there is no workspace), so drift there
//! ships a binary built differently from the released one.

use anyhow::{Context, Result};

use crate::coverage::Runner;

/// Path of the manifest holding every version declaration, relative to the
/// workspace root.
const MANIFEST: &str = "Cargo.toml";

/// Path of the `leviath-cli` manifest, relative to the workspace root.
///
/// The twelve declarations in the root manifest are not all of them: this one
/// carries a thirteenth (the allocator pin, deliberately kept out of the
/// workspace table) and a copy of `[profile.release]` that a `cargo install`
/// build depends on. Both are checked here, because both are invisible from
/// the root.
const CLI_MANIFEST: &str = "crates/leviath-cli/Cargo.toml";

/// Path of the changelog whose `## Unreleased` heading a bump rolls over.
const CHANGELOG: &str = "CHANGELOG.md";

/// Path of the published OpenAPI spec, whose `info.version` names the API this
/// build serves.
///
/// Bumped here because `API_VERSION` is the crate version and a test holds it
/// equal to this document. Keeping the two together is this command's job, not
/// the releaser's.
const SPEC: &str = "docs/schema/openapi.json";

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
        .filter(|line| is_workspace_path_dep(line))
        .filter_map(|line| {
            let name = line.split(" = ").next()?.trim();
            let pinned = pin_of(line)?;
            Some((name.to_owned(), pinned))
        })
        .collect()
}

/// Whether a dependency line names a crate in this workspace by path.
///
/// Two spellings, because the pins live in two manifests. The root's
/// `[workspace.dependencies]` reach down (`path = "crates/..."`), and
/// `leviath-cli`'s allocator pin reaches sideways (`path = "../..."`) - it is
/// kept out of the workspace table on purpose, so only the composition-root
/// binary picks an allocator. A third-party dependency carries no path at all,
/// so a path is enough to identify one of ours.
fn is_workspace_path_dep(line: &str) -> bool {
    line.contains("path = \"crates/") || line.contains("path = \"../")
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
    Ok(disagreeing_pins_against(manifest, &expected))
}

/// The same check against a version supplied from elsewhere, for a manifest
/// that inherits its version rather than declaring one.
pub fn disagreeing_pins_against(manifest: &str, expected: &str) -> Vec<String> {
    pinned_versions(manifest)
        .into_iter()
        .filter(|(_, pinned)| pinned != expected)
        .map(|(name, pinned)| format!("{name} pins {pinned}"))
        .collect()
}

/// The `[profile.release]` settings a manifest declares, as `(key, value)` in
/// declaration order, with comments and blank lines dropped.
///
/// A section ends at the next `[header]`, which is also how cargo reads it.
pub fn release_profile(manifest: &str) -> Vec<(String, String)> {
    manifest
        .lines()
        .skip_while(|line| line.trim() != "[profile.release]")
        .skip(1)
        .take_while(|line| !line.trim_start().starts_with('['))
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            Some((key.trim().to_owned(), value.trim().to_owned()))
        })
        .collect()
}

/// Report the release-profile settings that differ between the two manifests.
///
/// `leviath-cli` carries a copy of the root's `[profile.release]` because a
/// `cargo install leviath-cli` build has no workspace - that manifest is the
/// root there, and without the copy cargo's defaults apply, silently dropping
/// `overflow-checks` so token and budget arithmetic wraps instead of panicking.
/// The copy's comment says it must be kept in sync; this is what makes that
/// enforceable rather than aspirational.
pub fn release_profile_drift(root: &str, cli: &str) -> Vec<String> {
    let (root, cli) = (release_profile(root), release_profile(cli));
    let mut drift: Vec<String> = Vec::new();

    for (key, value) in &root {
        match cli.iter().find(|(k, _)| k == key) {
            Some((_, other)) if other == value => {}
            Some((_, other)) => drift.push(format!(
                "{key} is {value} at the root, {other} in {CLI_MANIFEST}"
            )),
            None => drift.push(format!("{key} = {value} is missing from {CLI_MANIFEST}")),
        }
    }
    for (key, value) in &cli {
        if !root.iter().any(|(k, _)| k == key) {
            drift.push(format!(
                "{key} = {value} in {CLI_MANIFEST} is not at the root"
            ));
        }
    }
    drift
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
        } else {
            out.push(rewrite_pin(line, new));
        }
    }

    anyhow::ensure!(
        rewrote_workspace,
        "no `version = \"...\"` line in the root Cargo.toml"
    );
    Ok(rejoin(manifest, out))
}

/// Rewrite every intra-workspace pin in a manifest that declares no version of
/// its own.
///
/// `leviath-cli`'s manifest inherits its version from the workspace, so it has
/// no `version = "..."` line to anchor on - but it does carry a pin, and that
/// pin has to move with the rest.
pub fn set_pins(manifest: &str, new: &str) -> Result<String> {
    validate_semver(new)?;
    let out = manifest.lines().map(|l| rewrite_pin(l, new)).collect();
    Ok(rejoin(manifest, out))
}

/// Swap the pinned version on a dependency line, or hand the line back as-is.
fn rewrite_pin(line: &str, new: &str) -> String {
    if is_workspace_path_dep(line) && pin_of(line).is_some() {
        replace_pin(line, new)
    } else {
        line.to_owned()
    }
}

/// Join rewritten lines back together, preserving whether the original ended
/// in a newline.
fn rejoin(original: &str, lines: Vec<String>) -> String {
    let mut text = lines.join("\n");
    if original.ends_with('\n') {
        text.push('\n');
    }
    text
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

/// Rewrite the OpenAPI spec's `info.version` to `new`.
///
/// Textual rather than a parse-and-reserialize, because the spec is a
/// hand-maintained document: round-tripping it through `serde_json` would
/// reformat every line of it and bury a one-line version bump in a diff nobody
/// can review.
///
/// Anchored to the `info` block's own indentation so it cannot hit the
/// `"version"` that appears inside a `required` array further down. Exactly one
/// line may match; zero means the spec moved and this needs updating, and more
/// than one means the anchor stopped being specific enough to trust.
pub fn set_spec_version(spec: &str, new: &str) -> Result<String> {
    let is_info_version =
        |line: &str| line.starts_with("    \"version\": \"") && line.trim_end().ends_with("\",");
    let matches = spec.lines().filter(|l| is_info_version(l)).count();
    anyhow::ensure!(
        matches == 1,
        "expected exactly one `info.version` line in {SPEC}, found {matches}"
    );
    let out = spec
        .lines()
        .map(|line| {
            if is_info_version(line) {
                format!("    \"version\": \"{new}\",")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    // `lines()` drops the trailing newline the file ends with; put it back
    // rather than leaving a no-newline-at-end-of-file diff on every bump.
    Ok(if spec.ends_with('\n') {
        format!("{out}\n")
    } else {
        out
    })
}

/// Rename `## Unreleased` to a dated heading for this version.
///
/// The entries themselves are left untouched: what was accumulating under
/// `## Unreleased` is exactly what this version ships.
///
/// No fresh `## Unreleased` is opened behind it: leviath.dev builds its release
/// notes from this file and rejects a section with no entries, so an empty
/// heading fails the docs check on the release PR itself. The next person to
/// write an entry adds the heading back, which the error below asks for by
/// name.
pub fn roll_changelog(changelog: &str, version: &str, date: &str) -> Result<String> {
    anyhow::ensure!(
        changelog.contains("\n## Unreleased\n"),
        "no `## Unreleased` heading in {CHANGELOG} - add one, with the entries this version ships, before bumping"
    );
    Ok(changelog.replacen(
        "\n## Unreleased\n",
        &format!("\n## {version} - {date}\n"),
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

// ── Release lists that must track the workspace ──────────────────────────────

/// Path of the workflow carrying the crates.io publish order.
const PROD_WORKFLOW: &str = ".github/workflows/prod.yml";

/// Path of the workflow carrying the per-package coverage matrix.
const CI_WORKFLOW: &str = ".github/workflows/ci.yml";

/// Members the coverage gate does not run, matching
/// [`crate::coverage::parse_workspace_packages`].
const UNGATED: &[&str] = &["xtask", "leviath-testkit", "leviath"];

/// Workspace member names, read from the root manifest's `members` list.
///
/// Parsed from the manifest rather than `cargo metadata` so this check costs
/// nothing and runs the same way offline.
fn workspace_members(manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_members = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("members") {
            in_members = true;
            continue;
        }
        if in_members {
            if trimmed.starts_with(']') {
                break;
            }
            if let Some(path) = trimmed
                .trim_matches(|c| c == '"' || c == ',')
                .strip_prefix("crates/")
            {
                names.push(path.to_owned());
            } else if let Some(name) = trimmed
                .strip_suffix("\",")
                .and_then(|s| s.strip_prefix('"'))
            {
                names.push(name.to_owned());
            }
        }
    }
    names
}

/// Crate names appearing in `text`, restricted to `candidates`.
///
/// Deliberately a containment test rather than a parse of the workflow's YAML:
/// the publish list is a shell `for` loop and the coverage list is a YAML
/// matrix, and what matters about both is only whether a name is in them.
fn names_present<'a>(text: &str, candidates: &'a [String]) -> Vec<&'a String> {
    candidates
        .iter()
        .filter(|name| {
            // Word-boundary-ish: `leviath` must not match inside `leviath-core`.
            text.split(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
                .any(|word| word == name.as_str())
        })
        .collect()
}

/// Members whose manifests carry `publish = false`, by name.
///
/// Read from the manifests rather than kept as a list here, so a crate that
/// opts out after the list was written is still seen. `leviath-alloc` did
/// exactly that: the flag went in as repo hygiene while the publish loop
/// still named it, and the 0.5.8 stable release spent thirty minutes retrying
/// a publish cargo refuses by design, then gave up with every crate behind it
/// unpublished.
fn opted_out_of_publish(manifests: &[(String, String)]) -> Vec<String> {
    manifests
        .iter()
        .filter(|(_, text)| text.lines().any(|line| line.trim() == "publish = false"))
        .map(|(name, _)| name.clone())
        .collect()
}

/// Every opted-out member the release still needs.
///
/// An opt-out breaks a release in two ways, both caught here rather than at
/// `cargo publish`: the prod workflow's loop names the crate, so the loop
/// fails on it; or a manifest pins it by version, so the crate naming it
/// cannot be verified against the registry. `pins` is `(manifest path,
/// dependency name)` for every intra-workspace pin.
fn unpublishable_but_needed(
    opted_out: &[String],
    prod: &str,
    pins: &[(String, String)],
) -> Vec<String> {
    let mut needed = Vec::new();
    for name in names_present(prod, opted_out) {
        needed.push(format!(
            "{name} is `publish = false` but {PROD_WORKFLOW}'s publish list names it, \
             and `cargo publish` refuses a crate that opted out"
        ));
    }
    for (manifest, dep) in pins {
        if opted_out.contains(dep) {
            needed.push(format!(
                "{manifest} pins {dep} by version, but {dep} is `publish = false`, \
                 so the crate naming it cannot be published"
            ));
        }
    }
    needed
}

/// Every member that must appear in a release list but does not.
///
/// The publish list and the coverage matrix are written out by hand in two
/// workflows, so nothing but this check ties them to the member list. A
/// publishable crate left off the publish list fails the release at `cargo
/// publish`, because a dependency carrying a version has to be on the registry
/// before the crate that names it. A release is the worst place to discover a
/// list is stale. `opted_out` names the members whose manifests say they are
/// never published; those are not demanded.
fn missing_from_release_lists(
    manifest: &str,
    prod: &str,
    ci: &str,
    opted_out: &[String],
) -> Vec<String> {
    let members = workspace_members(manifest);
    let mut missing = Vec::new();

    let publishable: Vec<String> = members
        .iter()
        .filter(|m| !opted_out.contains(m))
        .cloned()
        .collect();
    let listed = names_present(prod, &publishable);
    for name in &publishable {
        if !listed.contains(&name) {
            missing.push(format!("{name} is not in {PROD_WORKFLOW}'s publish list"));
        }
    }

    let gated: Vec<String> = members
        .iter()
        .filter(|m| !UNGATED.contains(&m.as_str()))
        .cloned()
        .collect();
    let listed = names_present(ci, &gated);
    for name in &gated {
        if !listed.contains(&name) {
            missing.push(format!("{name} is not in {CI_WORKFLOW}'s coverage matrix"));
        }
    }

    missing
}

/// Real entry point: reads and writes the workspace files.
pub fn run(mode: VersionMode) -> Result<()> {
    run_with(&crate::coverage::RealRunner, mode)
}

/// Entry point with the subprocess runner injected, so tests can drive the
/// `cargo update` step without spawning cargo.
pub fn run_with(runner: &dyn Runner, mode: VersionMode) -> Result<()> {
    let manifest = std::fs::read_to_string(MANIFEST)
        .with_context(|| format!("reading {MANIFEST} - run this from the workspace root"))?;
    let cli_manifest = std::fs::read_to_string(CLI_MANIFEST)
        .with_context(|| format!("reading {CLI_MANIFEST} - run this from the workspace root"))?;

    match mode {
        VersionMode::Check => {
            let expected = workspace_version(&manifest)?;
            let mut disagreeing = disagreeing_pins(&manifest)?;
            for name in disagreeing_pins_against(&cli_manifest, &expected) {
                disagreeing.push(format!("{name} (in {CLI_MANIFEST})"));
            }
            anyhow::ensure!(
                disagreeing.is_empty(),
                "[workspace.package] version is {expected}, but {}. \
                 Run `cargo xtask version <X.Y.Z>` rather than editing them by hand.",
                disagreeing.join(", ")
            );

            let drift = release_profile_drift(&manifest, &cli_manifest);
            anyhow::ensure!(
                drift.is_empty(),
                "{CLI_MANIFEST}'s [profile.release] has drifted from the root's: {}. \
                 The copy exists because `cargo install leviath-cli` builds without a \
                 workspace; the two must agree or the published binary is built \
                 differently from the released one.",
                drift.join("; ")
            );

            let prod = std::fs::read_to_string(PROD_WORKFLOW).unwrap_or_default();
            let ci = std::fs::read_to_string(CI_WORKFLOW).unwrap_or_default();
            let opted_out = opted_out_of_publish(&crate::structure::crate_manifests()?);
            let missing = missing_from_release_lists(&manifest, &prod, &ci, &opted_out);
            anyhow::ensure!(
                missing.is_empty(),
                "a workspace member is missing from a release list: {}. \
                 Add it, in dependency order for the publish list - a crate whose \
                 dependency is not on crates.io cannot be published.",
                missing.join("; ")
            );

            let mut pins: Vec<(String, String)> = pinned_versions(&manifest)
                .into_iter()
                .map(|(dep, _)| (MANIFEST.to_owned(), dep))
                .collect();
            pins.extend(
                pinned_versions(&cli_manifest)
                    .into_iter()
                    .map(|(dep, _)| (CLI_MANIFEST.to_owned(), dep)),
            );
            let needed = unpublishable_but_needed(&opted_out, &prod, &pins);
            anyhow::ensure!(
                needed.is_empty(),
                "a crate the release needs has opted out of publishing: {}. \
                 Drop the `publish = false`, or stop naming the crate.",
                needed.join("; ")
            );

            println!(
                "Every intra-workspace pin matches [workspace.package] version {expected}, \
                 both [profile.release] blocks agree, every member is in the \
                 publish list and the coverage matrix, and nothing the release \
                 needs has opted out of publishing."
            );
            Ok(())
        }
        VersionMode::Set(new) => {
            let previous = workspace_version(&manifest)?;
            std::fs::write(MANIFEST, set_versions(&manifest, &new)?)
                .with_context(|| format!("writing {MANIFEST}"))?;
            std::fs::write(CLI_MANIFEST, set_pins(&cli_manifest, &new)?)
                .with_context(|| format!("writing {CLI_MANIFEST}"))?;

            let changelog = std::fs::read_to_string(CHANGELOG)
                .with_context(|| format!("reading {CHANGELOG}"))?;
            std::fs::write(CHANGELOG, roll_changelog(&changelog, &new, &today())?)
                .with_context(|| format!("writing {CHANGELOG}"))?;

            // `API_VERSION` is `env!("CARGO_PKG_VERSION")`, and a test in
            // `serve/mod.rs` holds it equal to this document. Without this the
            // bump itself would fail that test.
            let spec = std::fs::read_to_string(SPEC).with_context(|| format!("reading {SPEC}"))?;
            std::fs::write(SPEC, set_spec_version(&spec, &new)?)
                .with_context(|| format!("writing {SPEC}"))?;

            // The lockfile records the workspace crates' own versions, and CI
            // rejects a lockfile that disagrees with the manifests.
            anyhow::ensure!(
                runner.cargo(&["update", "--workspace"])?,
                "cargo update --workspace failed; {MANIFEST}, {CHANGELOG} and {SPEC} were still written"
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

    /// A manifest shaped like `leviath-cli`'s: no version of its own, one
    /// sideways path pin, workspace-inherited deps, and a third-party pin that
    /// must never be rewritten.
    const CLI_MANIFEST_FIXTURE: &str = concat!(
        "[package]\n",
        "name = \"leviath-cli\"\n",
        "version.workspace = true\n",
        "\n",
        "[dependencies]\n",
        "mimalloc = { version = \"0.1\", optional = true }\n",
        "leviath-alloc = { path = \"../leviath-alloc\", version = \"0.1.2\", optional = true }\n",
        "leviath-core = { workspace = true }\n",
    );

    /// A manifest carrying a `[profile.release]` with prose between settings,
    /// as the real root one does.
    const MANIFEST_WITH_PROFILE: &str = concat!(
        "[profile.release]\n",
        "# Overflow panics instead of wrapping.\n",
        "overflow-checks = true\n",
        "\n",
        "# One codegen unit and fat LTO.\n",
        "lto = \"fat\"\n",
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

    /// The spec's own `info.version` line, rewritten; everything else,
    /// including the `"version"` that appears inside a `required` array, left
    /// exactly as it was.
    #[test]
    fn set_spec_version_rewrites_only_the_info_block() {
        let spec = concat!(
            "{\n",
            "  \"openapi\": \"3.1.0\",\n",
            "  \"info\": {\n",
            "    \"title\": \"Leviath HTTP API\",\n",
            "    \"version\": \"0.4.0\",\n",
            "    \"description\": \"d\"\n",
            "  },\n",
            "  \"components\": {\n",
            "    \"required\": [\n",
            "      \"version\"\n",
            "    ]\n",
            "  }\n",
            "}\n",
        );
        let out = set_spec_version(spec, "0.5.1").expect("one info.version line");
        assert!(out.contains("    \"version\": \"0.5.1\",\n"));
        assert!(!out.contains("0.4.0"));
        // The bare array entry is not a version declaration and is untouched.
        assert!(out.contains("      \"version\"\n"));
        assert!(
            out.ends_with("}\n"),
            "the trailing newline survives: {out:?}"
        );
    }

    /// A spec with no `info.version` line has moved out from under the anchor,
    /// and silently writing it back unchanged would hand the releaser a spec
    /// naming the previous build.
    #[test]
    fn set_spec_version_refuses_a_spec_with_no_version_line() {
        let err = set_spec_version("{\n  \"openapi\": \"3.1.0\"\n}\n", "0.5.1")
            .expect_err("nothing to rewrite");
        assert!(err.to_string().contains("found 0"), "{err}");
    }

    /// Two matching lines mean the anchor stopped being specific enough to
    /// trust, so it refuses rather than rewriting whichever it saw first.
    #[test]
    fn set_spec_version_refuses_an_ambiguous_spec() {
        let spec = "    \"version\": \"0.4.0\",\n    \"version\": \"0.4.0\",\n";
        let err = set_spec_version(spec, "0.5.1").expect_err("ambiguous");
        assert!(err.to_string().contains("found 2"), "{err}");
    }

    /// A spec that does not end in a newline gets none added, so the function
    /// is a faithful rewrite rather than a reformatter.
    #[test]
    fn set_spec_version_leaves_a_missing_trailing_newline_missing() {
        let out = set_spec_version("    \"version\": \"0.4.0\",", "0.5.1").expect("one line");
        assert_eq!(out, "    \"version\": \"0.5.1\",");
    }

    /// The real spec on disk is the one this has to work on, and it is the one
    /// that would otherwise only be exercised on release day.
    #[test]
    fn set_spec_version_rewrites_the_published_spec() {
        let spec = include_str!("../../docs/schema/openapi.json");
        let out = set_spec_version(spec, "9.9.9").expect("the published spec has one");
        assert!(out.contains("    \"version\": \"9.9.9\","));
        let parsed: serde_json::Value =
            serde_json::from_str(&out).expect("the rewrite is still valid JSON");
        assert_eq!(parsed["info"]["version"], "9.9.9");
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

    // ── The second manifest ───────────────────────────────────────────────────

    #[test]
    fn a_sideways_pin_is_seen_at_all() {
        let pins = pinned_versions(CLI_MANIFEST_FIXTURE);
        assert_eq!(pins, vec![("leviath-alloc".to_owned(), "0.1.2".to_owned())]);
    }

    #[test]
    fn a_stale_sideways_pin_is_reported() {
        let stale = CLI_MANIFEST_FIXTURE.replace("version = \"0.1.2\"", "version = \"0.1.1\"");
        assert_eq!(
            disagreeing_pins_against(&stale, "0.1.2"),
            vec!["leviath-alloc pins 0.1.1".to_owned()]
        );
    }

    #[test]
    fn a_workspace_inherited_dep_is_not_mistaken_for_a_pin() {
        // `leviath-core = { workspace = true }` carries neither path nor
        // version; reading it as a pin would report every such line as stale.
        assert!(
            !pinned_versions(CLI_MANIFEST_FIXTURE)
                .iter()
                .any(|(n, _)| n == "leviath-core")
        );
    }

    #[test]
    fn set_pins_moves_the_sideways_pin_and_leaves_everything_else() {
        let out = set_pins(CLI_MANIFEST_FIXTURE, "0.2.0").unwrap();
        assert!(out.contains("path = \"../leviath-alloc\", version = \"0.2.0\""));
        assert!(
            out.contains("mimalloc = { version = \"0.1\""),
            "third-party pin moved: {out}"
        );
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn set_pins_rejects_a_malformed_version() {
        assert!(set_pins(CLI_MANIFEST_FIXTURE, "nope").is_err());
    }

    #[test]
    fn set_pins_does_not_need_a_workspace_version_line() {
        // The whole reason this exists: leviath-cli inherits its version, so
        // `set_versions` would fail on it for want of an anchor.
        assert!(!CLI_MANIFEST_FIXTURE.contains("\nversion = \""));
        assert!(set_pins(CLI_MANIFEST_FIXTURE, "0.2.0").is_ok());
    }

    // ── Release-profile parity ────────────────────────────────────────────────

    #[test]
    fn release_profile_reads_settings_and_skips_prose() {
        assert_eq!(
            release_profile(MANIFEST_WITH_PROFILE),
            vec![
                ("overflow-checks".to_owned(), "true".to_owned()),
                ("lto".to_owned(), "\"fat\"".to_owned()),
            ]
        );
    }

    #[test]
    fn release_profile_stops_at_the_next_section() {
        let extended = format!("{MANIFEST_WITH_PROFILE}\n[profile.dev]\nopt-level = 1\n");
        assert!(
            !release_profile(&extended)
                .iter()
                .any(|(k, _)| k == "opt-level")
        );
    }

    #[test]
    fn release_profile_of_a_manifest_without_one_is_empty() {
        assert!(release_profile(MANIFEST).is_empty());
    }

    #[test]
    fn identical_profiles_do_not_drift() {
        assert!(release_profile_drift(MANIFEST_WITH_PROFILE, MANIFEST_WITH_PROFILE).is_empty());
    }

    #[test]
    fn a_changed_setting_is_reported_with_both_values() {
        let cli =
            MANIFEST_WITH_PROFILE.replace("overflow-checks = true", "overflow-checks = false");
        let drift = release_profile_drift(MANIFEST_WITH_PROFILE, &cli);
        assert_eq!(drift.len(), 1, "{drift:?}");
        assert!(
            drift[0].contains("true") && drift[0].contains("false"),
            "{drift:?}"
        );
    }

    #[test]
    fn a_setting_missing_from_the_copy_is_reported() {
        // A setting only the workspace manifest carries means `cargo install
        // leviath-cli` silently builds without overflow checks.
        let cli = MANIFEST_WITH_PROFILE.replace("overflow-checks = true\n", "");
        let drift = release_profile_drift(MANIFEST_WITH_PROFILE, &cli);
        assert_eq!(drift.len(), 1, "{drift:?}");
        assert!(drift[0].contains("missing"), "{drift:?}");
    }

    #[test]
    fn a_setting_only_in_the_copy_is_reported() {
        let cli = format!("{MANIFEST_WITH_PROFILE}panic = \"abort\"\n");
        let drift = release_profile_drift(MANIFEST_WITH_PROFILE, &cli);
        assert_eq!(drift.len(), 1, "{drift:?}");
        assert!(drift[0].contains("panic"), "{drift:?}");
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
    fn roll_changelog_dates_the_unreleased_heading_and_leaves_no_empty_one() {
        let rolled = roll_changelog(
            "# Changelog\n\n## Unreleased\n\n- did a thing\n\n## 0.1.1 - 2026-07-31\n",
            "0.1.2",
            "2026-08-02",
        )
        .unwrap();
        assert!(rolled.contains("## 0.1.2 - 2026-08-02\n\n- did a thing"));
        assert!(
            !rolled.contains("## Unreleased"),
            "an empty Unreleased fails the site's changelog check: {rolled}"
        );
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

    // ── Release lists track the workspace ────────────────────────────────────────

    const MEMBERS: &str = r#"
[workspace]
members = [
    "crates/leviath-core",
    "crates/leviath-net",
    "crates/leviath-alloc",
    "crates/leviath-testkit",
    "xtask",
]
"#;

    #[test]
    fn workspace_members_reads_the_member_list() {
        let members = workspace_members(MEMBERS);
        assert_eq!(
            members,
            vec![
                "leviath-core",
                "leviath-net",
                "leviath-alloc",
                "leviath-testkit",
                "xtask"
            ]
        );
    }

    /// The members whose manifests opt out of publishing, as the real
    /// workspace has them.
    fn opted_out() -> Vec<String> {
        vec!["xtask".to_owned(), "leviath-testkit".to_owned()]
    }

    /// A manifest with `publish = false` anywhere in it opts out; one without
    /// it, or with only a comment mentioning the flag, does not.
    #[test]
    fn opted_out_members_are_read_from_their_manifests() {
        let manifests = vec![
            (
                "leviath-alloc".to_owned(),
                "[package]\nname = \"leviath-alloc\"\npublish = false\n".to_owned(),
            ),
            (
                "leviath-core".to_owned(),
                "[package]\n# publish = false was considered\nname = \"leviath-core\"\n".to_owned(),
            ),
            (
                "xtask".to_owned(),
                "[package]\n  publish = false\n".to_owned(),
            ),
        ];
        assert_eq!(
            opted_out_of_publish(&manifests),
            vec!["leviath-alloc", "xtask"]
        );
    }

    /// An opted-out crate that the publish loop still names is reported, as is
    /// one that a manifest pins by version; an opt-out nothing needs is not.
    #[test]
    fn an_opted_out_crate_the_release_needs_is_reported() {
        let opted = vec!["leviath-alloc".to_owned(), "leviath-testkit".to_owned()];
        let pins = vec![
            ("Cargo.toml".to_owned(), "leviath-core".to_owned()),
            (
                "crates/leviath-cli/Cargo.toml".to_owned(),
                "leviath-alloc".to_owned(),
            ),
        ];
        let needed = unpublishable_but_needed(
            &opted,
            "for c in leviath-net leviath-alloc leviath-core; do",
            &pins,
        );
        assert_eq!(needed.len(), 2, "{needed:?}");
        assert!(
            needed[0].contains("leviath-alloc") && needed[0].contains("publish list"),
            "{needed:?}"
        );
        assert!(
            needed[1].contains("crates/leviath-cli/Cargo.toml") && needed[1].contains("pins"),
            "{needed:?}"
        );
        assert!(
            !needed.iter().any(|n| n.contains("testkit")),
            "a path-only opt-out nobody names is fine: {needed:?}"
        );
    }

    /// With the flag gone and the loop still naming the crate, nothing is
    /// reported: that is the shape a release needs.
    #[test]
    fn a_publishable_crate_in_the_loop_is_not_reported() {
        let needed = unpublishable_but_needed(
            &opted_out(),
            "for c in leviath-net leviath-alloc; do",
            &[("Cargo.toml".to_owned(), "leviath-alloc".to_owned())],
        );
        assert!(needed.is_empty(), "{needed:?}");
    }

    /// A member in neither workflow is reported once per list, so the message
    /// names the publish list and the coverage matrix separately rather than
    /// stopping at the first.
    #[test]
    fn a_member_missing_from_both_lists_is_reported_twice() {
        let missing = missing_from_release_lists(
            MEMBERS,
            "for c in leviath-core; do",
            "- leviath-core",
            &opted_out(),
        );
        assert!(
            missing
                .iter()
                .any(|m| m.contains("leviath-net") && m.contains("publish list")),
            "{missing:?}"
        );
        assert!(
            missing
                .iter()
                .any(|m| m.contains("leviath-net") && m.contains("coverage matrix")),
            "{missing:?}"
        );
    }

    /// Members whose manifests say they are never published, and the facade with
    /// no executable regions, are not expected in the lists - or the check would
    /// demand entries that must not exist.
    #[test]
    fn the_excluded_members_are_not_demanded() {
        let missing = missing_from_release_lists(
            MEMBERS,
            "for c in leviath-core leviath-net leviath-alloc; do",
            "- leviath-core\n- leviath-net\n- leviath-alloc",
            &opted_out(),
        );
        assert!(missing.is_empty(), "{missing:?}");
        assert!(
            !missing
                .iter()
                .any(|m| m.contains("xtask") || m.contains("testkit")),
            "{missing:?}"
        );
    }

    /// A name must match as a whole word: `leviath` appearing in the publish list
    /// must not satisfy `leviath-core`, or the check would pass on a list that is
    /// missing almost everything.
    #[test]
    fn a_substring_does_not_count_as_present() {
        let missing =
            missing_from_release_lists(MEMBERS, "for c in leviath; do", "- leviath", &opted_out());
        assert!(
            missing.iter().any(|m| m.contains("leviath-core")),
            "a prefix match must not satisfy a longer name: {missing:?}"
        );
    }
}
