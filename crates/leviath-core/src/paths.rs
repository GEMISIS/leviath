//! Path containment, and the one definition of where Leviath's data lives.

use std::path::{Path, PathBuf};

/// The user's home directory, honoring the `LEVIATH_HOME` override.
///
/// `dirs::home_dir()` cannot be redirected by `$HOME` on macOS
/// (`NSHomeDirectory()`) or `%USERPROFILE%` on Windows
/// (`SHGetKnownFolderPath`), so `LEVIATH_HOME` is the single override every
/// home-relative path honors - which is what lets a test (including one that
/// spawns the real `lev` binary) redirect all of them at once.
///
/// `None` when neither resolves; callers decide how to report that rather than
/// this silently picking a surprising fallback.
pub fn home_dir() -> Option<PathBuf> {
    if let Some(override_home) = std::env::var_os("LEVIATH_HOME") {
        return Some(PathBuf::from(override_home));
    }
    dirs::home_dir()
}

/// Leviath's data root: `<home>/.leviath`.
///
/// Every persistent thing Leviath owns lives under here - `config.toml`,
/// `mcp-auth.json`, `runs/`, `agents/`, `providers/`, `tools/`, the control
/// socket. Having one function say so is the point: there were seven separate
/// resolvers with **three incompatible readings of `LEVIATH_HOME`**, so with the
/// override set the MCP OAuth token store landed in a different directory from
/// the config that names those servers, and `lev serve` could not see agents
/// `lev add` had just installed.
pub fn data_dir() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".leviath"))
}

/// The directory scanned for global drop-in Rhai *tool* scripts.
///
/// `<home>/.leviath/tools`, alongside `providers/` and `agents/` - not
/// `<home>/tools`, which is where three call sites landed by joining `"tools"`
/// onto the *user home* rather than the data root. That mattered: every `.rhai`
/// file found here is compiled and offered to **every** agent as an executable
/// tool, and `$HOME/tools` is an ordinary, non-hidden directory a developer may
/// well already have or that other software may write into.
pub fn tools_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("tools"))
}

/// The directory holding drop-in Rhai *provider* scripts.
pub fn providers_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("providers"))
}

/// The directory installed agents are unpacked into.
pub fn agents_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("agents"))
}

/// Whether `name` is safe to use as a single path component.
///
/// Accepts `[A-Za-z0-9._-]+` and nothing else. Everything a caller supplies as
/// "the name of a thing" - a blueprint name from a REST body, a run id from a
/// URL segment - must pass this before it is `join`ed onto a directory, because
/// `Path::join` does not normalize and does not resist an absolute path:
///
/// ```text
/// agents_dir().join("../../../../tmp/x")   // escapes
/// agents_dir().join("/etc/cron.d/x")       // replaces the base entirely
/// ```
///
/// That was reachable from `POST /api/blueprints` (arbitrary directory creation
/// and manifest write) and `DELETE /api/blueprints/{name}` (arbitrary recursive
/// deletion, via a percent-encoded `..%2f` that decodes after segment matching).
///
/// A leading `.` is allowed (dotted names are ordinary) but `.` and `..`
/// themselves are not, since both are traversal rather than names.
pub fn is_safe_path_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Whether `path` still lands inside `root` once symlinks are followed.
///
/// A lexical `starts_with` check is not containment. It answers "does this
/// string begin with that string", and a symlink at `<root>/link` pointing at
/// `/` satisfies it while pointing anywhere on the filesystem. Every file tool
/// in Leviath was relying on exactly that check.
///
/// `path` need not exist - a `write_file` creating a new file is the common
/// case. The deepest *existing* ancestor is canonicalized (which is where any
/// symlink lives) and the unresolved tail re-appended, so a not-yet-created file
/// under a symlinked parent is still caught.
///
/// `root` is canonicalized here too rather than trusted: on macOS `/tmp` is a
/// symlink to `/private/tmp`, so a root that was stored uncanonicalized would
/// never prefix-match a canonicalized path and every access would be refused.
///
/// This is not TOCTOU-proof - a symlink planted between this call and the
/// subsequent `open` still wins. Closing that needs `openat`/`O_NOFOLLOW`
/// throughout. This stops the planted-symlink case, which is the one an agent
/// can arrange for itself.
pub fn resolves_within(path: &Path, root: &Path) -> bool {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    match canonicalize_existing_prefix(path) {
        Some(real) => real.starts_with(&root),
        // Nothing along the path could be canonicalized at all (no existing
        // ancestor, not even the filesystem root). Refuse: an unverifiable path
        // is not a safe one.
        None => false,
    }
}

/// Canonicalize a path for allowlist matching, failing closed.
///
/// The same machinery [`resolves_within`] uses, exposed for the `[read_paths]`
/// resolver in `leviath-tools`: the deepest existing ancestor is
/// canonicalized (which is where any symlink lives) and the unresolved tail
/// re-appended. `None` means nothing along the path could be verified, and an
/// unverifiable path must be refused, never matched.
pub fn canonicalize_for_match(path: &Path) -> Option<PathBuf> {
    canonicalize_existing_prefix(path)
}

/// Canonicalize the deepest existing ancestor of `path` and re-append whatever
/// tail did not exist, so a path to a not-yet-created file still has any
/// symlinks in its parents resolved.
fn canonicalize_existing_prefix(path: &Path) -> Option<PathBuf> {
    let mut probe = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(&probe) {
            let mut full = real;
            // `tail` was pushed leaf-first while walking up, so replay it in
            // reverse to rebuild the original order.
            for component in tail.iter().rev() {
                full.push(component);
            }
            return Some(full);
        }
        // Both in one step: after `file_name()` succeeds, `parent()` cannot
        // fail (only a root has no parent, and a root has no file name either),
        // so two separate `?`s would leave the second permanently uncovered.
        let (Some(name), Some(parent)) = (probe.file_name(), probe.parent()) else {
            return None;
        };
        let (name, parent) = (name.to_os_string(), parent.to_path_buf());
        tail.push(name);
        probe = parent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything Leviath persists sits under one root, and `LEVIATH_HOME`
    /// moves all of it together. Seven separate resolvers with three readings of
    /// that variable is what this replaced - and the consequence was concrete: a
    /// run that believed it was isolated wrote to the real
    /// `~/.leviath/config.toml`.
    #[test]
    fn every_data_path_follows_leviath_home_together() {
        temp_env::with_var("LEVIATH_HOME", Some("/tmp/lev-paths-test"), || {
            let root = PathBuf::from("/tmp/lev-paths-test");
            assert_eq!(home_dir(), Some(root.clone()));
            let data = root.join(".leviath");
            assert_eq!(data_dir(), Some(data.clone()));
            assert_eq!(tools_dir(), Some(data.join("tools")));
            assert_eq!(providers_dir(), Some(data.join("providers")));
            assert_eq!(agents_dir(), Some(data.join("agents")));
        });
    }

    /// Without the override, everything is under the real home's `.leviath`.
    /// Asserted by shape: CI always has a home, but which one is not this
    /// module's business.
    #[test]
    fn without_the_override_paths_sit_under_the_real_home() {
        temp_env::with_var_unset("LEVIATH_HOME", || {
            let home = home_dir().expect("a home directory resolves");
            let data = data_dir().expect("so does the data dir");
            assert_eq!(data, home.join(".leviath"));
            // The global tool-scan directory in particular must sit inside
            // Leviath's own directory, never a bare `$HOME/tools`: every
            // `.rhai` file there becomes an executable tool for every agent.
            assert_eq!(tools_dir(), Some(data.join("tools")));
            assert!(tools_dir().expect("set").ends_with(".leviath/tools"));
        });
    }

    #[test]
    fn safe_components_are_ordinary_names() {
        for name in ["coder", "my-agent", "agent_2", "v1.2.3", ".hidden", "a"] {
            assert!(is_safe_path_component(name), "{name}");
        }
    }

    #[test]
    fn traversal_and_separators_are_refused() {
        for name in [
            "",
            ".",
            "..",
            "../evil",
            "../../../../tmp/x",
            "/etc/passwd",
            "a/b",
            "a\\b",
            // Percent-encoding decodes before this is called; the decoded form
            // is what must be rejected.
            "..%2fevil",
            // NUL and other control characters truncate paths in C APIs.
            "a\0b",
            "a b",
            "a;rm -rf",
            // A drive-relative Windows path.
            "C:evil",
        ] {
            assert!(!is_safe_path_component(name), "{name:?} should be refused");
        }
    }

    #[test]
    fn plain_paths_inside_the_root_are_contained() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("file.txt"), b"x").unwrap();
        assert!(resolves_within(&root.join("file.txt"), root));
        // A file that does not exist yet is still contained.
        assert!(resolves_within(&root.join("new.txt"), root));
        // ...including under directories that do not exist yet.
        assert!(resolves_within(&root.join("a/b/c.txt"), root));
    }

    #[test]
    fn a_path_outside_the_root_is_not_contained() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        assert!(!resolves_within(other.path(), dir.path()));
    }

    /// The case a lexical `starts_with` check cannot see: the path is textually
    /// inside the root the whole way, and still reads `/etc/passwd`.
    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_root_is_not_contained() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let link = root.join("link");
        std::os::unix::fs::symlink("/", &link).unwrap();

        // Lexically this is impeccable - and that was the whole problem.
        let target = link.join("etc/passwd");
        assert!(
            target.starts_with(root),
            "precondition: textually contained"
        );
        assert!(!resolves_within(&target, root));
    }

    /// A symlink whose target is *inside* the root is fine - the check is about
    /// where the path lands, not whether a symlink was involved.
    #[cfg(unix)]
    #[test]
    fn a_symlink_within_the_root_is_contained() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("real")).unwrap();
        std::fs::write(root.join("real/file.txt"), b"x").unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        assert!(resolves_within(&root.join("link/file.txt"), root));
    }

    /// A not-yet-created file *under* an escaping symlink is caught too: the
    /// symlink is in the parent, which is where canonicalization starts.
    #[cfg(unix)]
    #[test]
    fn a_new_file_under_an_escaping_symlink_is_not_contained() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
        assert!(!resolves_within(&root.join("link/brand-new.txt"), root));
    }

    /// macOS puts temp dirs under a symlinked `/tmp`, so an uncanonicalized root
    /// must still work - canonicalizing only the path and not the root would
    /// refuse every access on that platform.
    #[test]
    fn an_uncanonicalized_root_still_matches() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("f.txt"), b"x").unwrap();
        // `dir.path()` is already whatever the OS handed us; canonicalizing the
        // path alone would diverge from it on macOS.
        assert!(resolves_within(&root.join("f.txt"), root));
    }

    /// The `[read_paths]` resolver's canonicalizer is the same machinery as
    /// `resolves_within`, exposed fail-closed: a real path resolves (symlinks
    /// and all), an unverifiable one is `None`.
    #[test]
    fn canonicalize_for_match_resolves_real_paths_and_refuses_unverifiable_ones() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
        assert_eq!(
            canonicalize_for_match(&dir.path().join("f.txt")),
            Some(std::fs::canonicalize(dir.path().join("f.txt")).unwrap())
        );
        assert_eq!(
            canonicalize_for_match(Path::new("no-such-relative-name")),
            None
        );
    }

    #[test]
    fn a_relative_path_with_no_existing_ancestor_is_refused() {
        // A bare relative name has no existing ancestor to canonicalize once the
        // parent chain runs out, so it cannot be verified and is refused.
        assert!(!resolves_within(
            Path::new("nonexistent-relative"),
            Path::new("/definitely/not/here")
        ));
    }
}
