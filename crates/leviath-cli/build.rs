//! Embeds a build identifier (`LEVIATH_BUILD`) so a running daemon can tell
//! whether the installed CLI is a newer build than itself and restart cleanly.
//!
//! The id is the short git commit hash plus a `-dirty` suffix when the working
//! tree has uncommitted changes, falling back to the package version when git
//! is unavailable (e.g. a packaged crate).

use std::process::Command;

fn main() {
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let build = match hash {
        Some(hash) => {
            let dirty = Command::new("git")
                .args(["status", "--porcelain"])
                .output()
                .ok()
                .map(|o| !o.stdout.is_empty())
                .unwrap_or(false);
            format!("{hash}{}", if dirty { "-dirty" } else { "" })
        }
        None => format!("v{}", env!("CARGO_PKG_VERSION")),
    };

    println!("cargo:rustc-env=LEVIATH_BUILD={build}");
    // Re-run when the checked-out commit or staged set changes.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
}
