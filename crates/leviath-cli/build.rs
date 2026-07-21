//! Embeds a build identifier (`LEVIATH_BUILD`) so a running daemon can tell
//! whether the installed CLI is a newer build than itself and restart cleanly.
//!
//! The id is the short git commit hash. When the working tree is dirty it also
//! carries a short hash of the uncommitted changes, so **every edit produces a
//! distinct id** — that's what lets a dev-iteration reinstall (same commit,
//! changed code) be detected as stale and reload the daemon. Falls back to the
//! package version when git is unavailable (e.g. a packaged crate).
//!
//! The script re-runs on every build (via a sentinel `rerun-if-changed` path
//! that never exists) so the dirty hash tracks source edits, not just commits;
//! when the computed id is unchanged this is free (Cargo won't recompile).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::process::Command;

/// Run a git command and return its stdout bytes, or `None` on any failure.
fn git(args: &[&str]) -> Option<Vec<u8>> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| o.stdout)
}

/// A short hex hash of the working tree's uncommitted state (the tracked diff
/// plus the porcelain status, which surfaces new/removed files), so the id moves
/// whenever the code does. `None` when the tree is clean.
fn dirty_hash() -> Option<String> {
    let status = git(&["status", "--porcelain"])?;
    if status.is_empty() {
        return None; // clean tree
    }
    let diff = git(&["diff", "HEAD"]).unwrap_or_default();
    let mut hasher = DefaultHasher::new();
    status.hash(&mut hasher);
    diff.hash(&mut hasher);
    Some(format!("{:08x}", hasher.finish() & 0xffff_ffff))
}

fn main() {
    let hash = git(&["rev-parse", "--short", "HEAD"])
        .and_then(|o| String::from_utf8(o).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let build = match hash {
        Some(hash) => match dirty_hash() {
            Some(dirty) => format!("{hash}-dirty-{dirty}"),
            None => hash,
        },
        None => format!("v{}", env!("CARGO_PKG_VERSION")),
    };

    println!("cargo:rustc-env=LEVIATH_BUILD={build}");
    // Re-run on every build so the dirty hash reflects the current source, not
    // just the last commit. A path that never exists is always "changed", which
    // forces the re-run; when LEVIATH_BUILD is unchanged Cargo skips recompiling.
    println!("cargo:rerun-if-changed=.leviath-build-always-rerun");
}
