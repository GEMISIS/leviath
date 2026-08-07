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
//! therefore stops at the first column-zero `#[cfg(test)]`, and sibling test
//! files (`tests.rs`, `*_tests.rs`, anything under a `tests/` directory) are
//! skipped outright.
//!
//! # One number, no exemptions
//!
//! The cap started at 800 with a table of files already above it, because a cap
//! the tree does not meet cannot go green and turns into an allowlist that only
//! grows. That table is gone: the files it named were split, and the cap is now
//! a number every file actually meets.
//!
//! It is a **ratchet**. 1,200 is where the tree sits today, not where it should
//! end up - the next rungs are 1,000 and 800, each earned by splitting the files
//! above it. The number only ever goes down.

use anyhow::{Result, bail};
use std::path::Path;

/// The most production lines a file may hold.
///
/// 1,200 is what the tree meets today, with the longest file at 1,152 and a
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
/// Everything from a column-zero `#[cfg(test)]` onward is test scaffolding. The
/// attribute has to be at column zero: an indented one sits on a field or a
/// method and does not begin the file's test section.
pub fn production_lines(source: &str) -> usize {
    source
        .lines()
        .position(|l| l.starts_with("#[cfg(test)]"))
        .unwrap_or_else(|| source.lines().count())
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

/// Run the check.
pub fn run(mode: StructureMode) -> Result<()> {
    let paths = walk_workspace()?;
    let measured = measure(&paths, &|p| Ok(std::fs::read_to_string(p)?))?;
    report(mode, &measured)
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
