//! Structural limits the workspace enforces mechanically.
//!
//! One rule today: a source file may hold at most [`MAX_PRODUCTION_LINES`] lines
//! of production code. It is checked here rather than by clippy because clippy
//! has no file-length lint, and here rather than by ast-grep because the answer
//! is a line count rather than a syntax match.
//!
//! # Why production lines rather than total lines
//!
//! This workspace gates a hard 100% and keeps most tests inline, so test code is
//! roughly two thirds of the tree. A total-lines rule would fire on the files
//! with the *best* test coverage, which is precisely backwards. The count
//! therefore skips every column-zero `#[cfg(test)]` item - the attribute stack,
//! then either a `;`-terminated one-liner or a braced item closing at column
//! zero - and keeps counting after it. Sibling test files (`tests.rs`,
//! `*_tests.rs`, anything under a `tests/` directory) are skipped outright.
//!
//! An *indented* `#[cfg(test)]` still counts as production: it sits on a field
//! or a method, and brace-matching arbitrary nesting is a parser this rule has
//! no business being. That over-reports a handful of files, which is the safe
//! direction for a ratchet.
//!
//! # One number, no exemptions
//!
//! The cap applies to every file, with no allowlist. A cap the tree does not
//! meet cannot go green, and an exemption table only ever grows, so the number
//! is one every file actually meets.
//!
//! It is a **ratchet**. 1,200 is where the tree sits today, not where it should
//! end up - the next rungs are 1,000 and 800, each earned by splitting the files
//! above it. The number only ever goes down.

use anyhow::{Result, bail};
use std::path::Path;

/// The most production lines a file may hold.
///
/// 1,200 is what the tree meets today, with the longest file at 1,184 and a
/// median of 218. Lower it as files get split; it must never go up. Raising it
/// to admit one long file is how a limit stops being one.
pub const MAX_PRODUCTION_LINES: usize = 1_200;

/// What `cargo xtask structure` was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructureMode {
    /// Report every file over its limit and fail if there are any.
    Check,
    /// Print the current production-line count of every file, longest first.
    List,
}

impl StructureMode {
    /// Parse the arguments after the subcommand.
    pub fn parse(args: &[String]) -> Result<Self> {
        match args.first().map(String::as_str) {
            None | Some("--check") => Ok(Self::Check),
            Some("--list") => Ok(Self::List),
            Some(other) => bail!("unknown argument for `structure`: {other}"),
        }
    }
}

/// Whether this path holds tests rather than production code.
///
/// Mirrors cargo-llvm-cov's own default exclusion, so "what the coverage gate
/// measures" and "what this rule counts" cannot drift apart.
pub fn is_test_path(path: &str) -> bool {
    let name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    name == "tests.rs" || name.ends_with("_tests.rs") || path.contains("/tests/")
}

/// How many production lines `source` holds.
///
/// Every column-zero `#[cfg(test)]` item is skipped and the count resumes
/// after it. The attribute has to be at column zero: an indented one sits on a
/// field or a method and is counted as production.
pub fn production_lines(source: &str) -> usize {
    let lines: Vec<&str> = source.lines().collect();
    let mut count = 0;
    let mut i = 0;
    while i < lines.len() {
        if starts_test_item(lines[i]) {
            i = past_test_item(&lines, i);
            continue;
        }
        count += 1;
        i += 1;
    }
    count
}

/// Whether this column-zero line is the first attribute of a test-only item.
///
/// `#[cfg(all(test, ...))]` counts too: `leviath-alloc` gates a test module
/// that way, and it is test scaffolding exactly like the plain form.
fn starts_test_item(line: &str) -> bool {
    line.starts_with("#[cfg(test)]") || line.starts_with("#[cfg(all(test")
}

/// The index just past the test-only item whose first attribute is at `at`.
///
/// Stacked attributes (`#[cfg(test)]` then `#[rustfmt::skip]`) are skipped
/// first. A `;`-terminated line is a one-liner (`mod tests;`, `use x::Y;`); any
/// other item is bracketed and, being at column zero, closes at column zero
/// with `}` (or the exact `];` / `);` a `static` slice or tuple ends in).
fn past_test_item(lines: &[&str], at: usize) -> usize {
    let mut i = at + 1;
    while i < lines.len() && lines[i].starts_with("#[") {
        i += 1;
    }
    let Some(item) = lines.get(i) else {
        return lines.len();
    };
    if item.trim_end().ends_with(';') {
        return i + 1;
    }
    let mut j = i;
    while j < lines.len() && !closes_item(lines[j]) {
        j += 1;
    }
    j + 1
}

/// Whether a column-zero line ends a bracketed item.
///
/// `}` closes a block, `];` a `static` slice and `);` a tuple struct. Only the
/// exact two-character forms are accepted for the latter: a raw string inside a
/// test module can put a bare `)` or `]` at column zero, and matching on the
/// first character alone ends the item there, counting the rest of the test
/// module as production.
fn closes_item(line: &str) -> bool {
    line.starts_with('}') || line == "];" || line == ");"
}

/// One file and how many production lines it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Measured {
    /// Workspace-relative path.
    pub path: String,
    /// Production lines it holds.
    pub lines: usize,
}

impl Measured {
    /// Whether this file is over [`MAX_PRODUCTION_LINES`].
    pub fn is_over(&self) -> bool {
        self.lines > MAX_PRODUCTION_LINES
    }
}

/// Measure every non-test `.rs` file the walker yields.
///
/// Takes the file list and a reader so the whole rule is testable without a
/// filesystem: the real caller passes `walk_workspace` and `std::fs::read_to_string`.
pub fn measure(paths: &[String], read: &dyn Fn(&str) -> Result<String>) -> Result<Vec<Measured>> {
    let mut out = Vec::new();
    for path in paths {
        if is_test_path(path) {
            continue;
        }
        let lines = production_lines(&read(path)?);
        out.push(Measured {
            path: path.clone(),
            lines,
        });
    }
    out.sort_by_key(|m| std::cmp::Reverse(m.lines));
    Ok(out)
}

/// Crates allowed to restate `[workspace.lints]` instead of inheriting it, with
/// the single lint each may drop.
///
/// Cargo rejects inheriting and overriding in one manifest, so a crate that must
/// escape one lint has to copy the whole table, and a copied table silently
/// drops every lint the root gains afterwards. Anything on this list is checked
/// against the root table, so the lint named here is the only difference
/// allowed.
const LINT_OPT_OUTS: &[(&str, &str)] = &[("leviath-alloc", "unsafe_code")];

/// The lint names a manifest declares under `[<prefix>.rust]` / `[<prefix>.clippy]`.
fn declared_lints(manifest: &str, prefix: &str) -> Vec<String> {
    let rust = format!("[{prefix}.rust]");
    let clippy = format!("[{prefix}.clippy]");
    let mut out = Vec::new();
    let mut inside = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            inside = trimmed == rust || trimmed == clippy;
        } else if inside
            && !trimmed.starts_with('#')
            && let Some((key, _)) = trimmed.split_once(" = ")
        {
            out.push(key.to_string());
        }
    }
    out
}

/// Whether a manifest opts into the workspace lint table.
fn inherits_workspace_lints(manifest: &str) -> bool {
    let mut inside = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            inside = trimmed == "[lints]";
        } else if inside && trimmed == "workspace = true" {
            return true;
        }
    }
    false
}

/// Crates that neither inherit `[workspace.lints]` nor restate it in full.
///
/// Pure so the rule is testable on synthetic manifests: `crates` is
/// `(name, manifest text)`.
pub fn lint_gaps(root_manifest: &str, crates: &[(String, String)]) -> Vec<String> {
    let expected = declared_lints(root_manifest, "workspace.lints");
    let mut gaps = Vec::new();
    for (name, manifest) in crates {
        if inherits_workspace_lints(manifest) {
            continue;
        }
        let Some((_, exempt)) = LINT_OPT_OUTS.iter().find(|(c, _)| c == name) else {
            gaps.push(format!(
                "{name} has no `[lints] workspace = true`, so none of the workspace lints \
                 apply to it"
            ));
            continue;
        };
        let declared = declared_lints(manifest, "lints");
        gaps.extend(
            expected
                .iter()
                .filter(|lint| lint.as_str() != *exempt && !declared.contains(lint))
                .map(|lint| {
                    format!(
                        "{name} restates the lint table but omits `{lint}`; it is only \
                         exempt from `{exempt}`"
                    )
                }),
        );
    }
    gaps
}

/// Run the check.
pub fn run(mode: StructureMode) -> Result<()> {
    let paths = walk_workspace()?;
    let measured = measure(&paths, &|p| Ok(std::fs::read_to_string(p)?))?;
    report(mode, &measured)?;
    if mode == StructureMode::List {
        return Ok(());
    }
    let gaps = lint_gaps(&std::fs::read_to_string("Cargo.toml")?, &crate_manifests()?);
    for gap in &gaps {
        eprintln!("[structure] {gap}");
    }
    if !gaps.is_empty() {
        bail!(
            "{} crate(s) do not carry the workspace lints. Give the manifest a `[lints]` \
             table with `workspace = true`, or - only if the crate must escape one lint - \
             restate the root table without that one.",
            gaps.len()
        );
    }
    println!("structure: every crate carries the workspace lints");
    Ok(())
}

/// `(crate name, manifest text)` for every member under `crates/`, plus `xtask`.
///
/// `xtask` is a workspace member like any other and carries the same lint
/// table; leaving it out here would let it drop the table without a word.
pub(crate) fn crate_manifests() -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("crates")?.flatten() {
        let manifest = entry.path().join("Cargo.toml");
        if manifest.is_file() {
            out.push((
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read_to_string(&manifest)?,
            ));
        }
    }
    out.push((
        "xtask".to_string(),
        std::fs::read_to_string("xtask/Cargo.toml")?,
    ));
    out.sort();
    Ok(out)
}

/// Render the outcome, and fail when something is over.
///
/// Split from [`run`] so the reporting is testable without walking a real tree.
pub fn report(mode: StructureMode, measured: &[Measured]) -> Result<()> {
    if mode == StructureMode::List {
        for m in measured {
            println!("{:>6}  {}", m.lines, m.path);
        }
        return Ok(());
    }

    let over: Vec<&Measured> = measured.iter().filter(|m| m.is_over()).collect();
    for m in &over {
        eprintln!(
            "[structure] {} holds {} production lines, over the {MAX_PRODUCTION_LINES} limit",
            m.path, m.lines
        );
    }
    if !over.is_empty() {
        bail!(
            "{} file(s) over the production-line limit. Split by concern - `config/`, \
             `blueprint/`, `host/` and `daemon/spawn/` are what that looks like. Raising \
             the cap to admit one long file is how a limit stops being one.",
            over.len()
        );
    }
    println!(
        "structure: {} files, none over the {MAX_PRODUCTION_LINES}-line limit",
        measured.len()
    );
    Ok(())
}

/// Every `.rs` file under `crates/` and `xtask/`, workspace-relative.
fn walk_workspace() -> Result<Vec<String>> {
    let mut out = Vec::new();
    for root in ["crates", "xtask"] {
        collect(Path::new(root), &mut out)?;
    }
    out.sort();
    Ok(out)
}

/// Recursively collect `.rs` paths under `dir`, skipping build output.
fn collect(dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            collect(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
