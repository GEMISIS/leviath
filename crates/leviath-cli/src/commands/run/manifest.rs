//! Filesystem discovery of `agent.leviath` manifests.
//!
//! The pure `TOML` -> [`leviath_core::Blueprint`] parser lives in
//! [`leviath_core::manifest`]; it is re-exported here as [`parse_manifest`] so
//! `super::manifest::parse_manifest` call sites resolve.

use std::path::{Path, PathBuf};

/// Resolve an agent argument to the `agent.leviath` file it names.
///
/// Accepts the file itself, a directory containing one, or an installed
/// agent's name.
pub(crate) fn find_manifest(path: &str) -> anyhow::Result<PathBuf> {
    let p = Path::new(path);

    // 1. Explicit agent.leviath file
    if p.is_file()
        && p.file_name() == Some(std::ffi::OsStr::new(leviath_core::files::MANIFEST_FILENAME))
    {
        return Ok(p.to_path_buf());
    }

    // 2. Directory with agent.leviath inside
    if p.is_dir() {
        let manifest = p.join(leviath_core::files::MANIFEST_FILENAME);
        if manifest.exists() {
            return Ok(manifest);
        }
    }

    // 3. Installed agent by name: <agents_dir>/<name>/agent.leviath.
    if let Some(installed) = installed_manifest(path) {
        return Ok(installed);
    }

    // 4. agent.leviath in current directory (for `lev run` with no path)
    let current_manifest = PathBuf::from(leviath_core::files::MANIFEST_FILENAME);
    if current_manifest.exists() {
        return Ok(current_manifest);
    }

    anyhow::bail!(
        "Could not find agent manifest for '{}'. \
        Pass a path to a directory containing agent.leviath, \
        or an installed agent name (see `lev list`).",
        path
    )
}

/// The manifest of an installed agent, `<agents_dir>/<name>/agent.leviath`,
/// when that file exists.
///
/// Resolved through the shared `LEVIATH_HOME`-aware helper, so `lev run <name>`
/// and `lev validate <name>` find the same install tree `lev add` writes when
/// the override is set. Never consults the current directory: a caller that
/// wants a typo to stay an error (`lev validate <typo>` run inside an agent
/// directory) uses this rather than [`find_manifest`].
pub(crate) fn installed_manifest(name: &str) -> Option<PathBuf> {
    leviath_core::paths::agents_dir()
        .map(|d| d.join(name).join(leviath_core::files::MANIFEST_FILENAME))
        .filter(|p| p.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    // `set_current_dir` is process-global, so any test whose assertion
    // implicitly depends on CWD state (like `find_manifest`'s "no
    // agent.leviath in CWD" branch) must serialize against every other
    // CWD-mutating test in the crate, not just the ones in this file --
    // otherwise it can observe a CWD another test temporarily pointed
    // elsewhere and fail nondeterministically. Confirmed exactly this:
    // `find_manifest_dir_without_manifest_falls_through` didn't hold a lock
    // and intermittently failed on CI by observing
    // `find_manifest_cwd_agent_leviath_found`'s CWD mid-swap. Uses the
    // crate-wide `crate::config::CWD_LOCK` (not a file-local one) so it
    // actually serializes against CWD-mutating tests added to other files.
    use crate::config::CWD_LOCK;

    // ─── find_manifest ───────────────────────────────────────────────────────

    #[test]
    fn find_manifest_with_file_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let manifest = dir.join("agent.leviath");
        std::fs::write(&manifest, "[agent]\nname = \"test\"").unwrap();

        let result = find_manifest(manifest.to_str().unwrap());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), manifest);
    }

    #[test]
    fn find_manifest_with_directory_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let manifest = dir.join("agent.leviath");
        std::fs::write(&manifest, "[agent]\nname = \"test\"").unwrap();

        let result = find_manifest(dir.to_str().unwrap());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), manifest);
    }

    #[test]
    fn find_manifest_with_invalid_path() {
        // See CWD_LOCK's doc comment - branch 4 depends on CWD state.
        let _guard = CWD_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = find_manifest("/nonexistent/path/to/nothing");
        assert!(result.is_err());
    }

    /// Name lookup goes through the `LEVIATH_HOME`-aware agents dir, so
    /// `lev run <name>` finds the same tree `lev add` installs into when the
    /// override is set. Resolving through the raw OS home would make
    /// installed-by-name agents invisible in any redirected environment (and
    /// force this very test to write into the developer's real home).
    #[test]
    fn find_manifest_installed_agent_by_name_honors_leviath_home() {
        let home = tempfile::tempdir().unwrap();
        let agent_dir = home.path().join(".leviath").join("agents").join("named");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let manifest_path = agent_dir.join("agent.leviath");
        std::fs::write(&manifest_path, "[agent]\nname = \"test\"").unwrap();

        temp_env::with_var("LEVIATH_HOME", Some(home.path()), || {
            assert_eq!(find_manifest("named").unwrap(), manifest_path);
        });
    }

    /// `installed_manifest` is the piece of `find_manifest` that other
    /// commands (`lev validate <name>`) reuse on its own: it answers the
    /// install tree and nothing else, so a name that is not installed is
    /// `None` even when the current directory holds a manifest.
    #[test]
    fn installed_manifest_answers_only_from_the_install_tree() {
        let home = tempfile::tempdir().unwrap();
        let agent_dir = home.path().join(".leviath").join("agents").join("named");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let manifest_path = agent_dir.join("agent.leviath");
        std::fs::write(&manifest_path, "[agent]\nname = \"test\"").unwrap();

        temp_env::with_var("LEVIATH_HOME", Some(home.path()), || {
            assert_eq!(installed_manifest("named"), Some(manifest_path.clone()));
            assert_eq!(installed_manifest("unnamed"), None);
        });
    }

    /// Covers branch 2 (directory exists) when the directory has NO `agent.leviath` inside.
    /// This exercises the implicit else of `if manifest.exists()` on line ~23.
    #[test]
    fn find_manifest_dir_without_manifest_falls_through() {
        // Branch 4 of find_manifest checks for a bare "agent.leviath" in the
        // process's current working directory - process-global state that
        // find_manifest_cwd_agent_leviath_found deliberately mutates. Without
        // holding CWD_LOCK here too, this test can run while CWD is
        // temporarily pointed at that other test's directory (which does
        // contain agent.leviath), observe branch 4 succeed, and fail this
        // assertion nondeterministically - confirmed on CI.
        let _guard = CWD_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        // No agent.leviath inside - the dir branch falls through to the error.
        let result = find_manifest(dir.to_str().unwrap());
        assert!(result.is_err());
    }

    /// Covers branch 3 (installed agent by name) when the agent is NOT installed.
    /// The `if let Some(home)` block is entered (home exists on macOS) but the
    /// `if installed.exists()` is false, so we fall through to the error.
    #[test]
    fn find_manifest_installed_agent_not_found_falls_through() {
        // See CWD_LOCK's doc comment - branch 4 depends on CWD state.
        let _guard = CWD_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = find_manifest("lev-no-such-agent-xyzzy-9f3a");
        assert!(result.is_err());
    }

    /// Covers branch 4: a bare `agent.leviath` exists in the current directory.
    /// Uses `CWD_LOCK` to prevent parallel tests from interfering.
    #[test]
    fn find_manifest_cwd_agent_leviath_found() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let manifest = dir.join("agent.leviath");
        std::fs::write(&manifest, "[agent]\nname = \"cwd-test\"").unwrap();

        // Serialize all CWD-mutating tests so they don't interfere.
        let _guard = CWD_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();

        // find_manifest("__nonexistent__") falls through branches 1-3 and
        // finds the agent.leviath in the new CWD (branch 4).
        let result = find_manifest("__lev_cwd_test_nonexistent__");

        // Always restore CWD before asserting so cleanup runs even on failure.
        std::env::set_current_dir(&original_cwd).unwrap();

        assert!(result.is_ok());
        assert_eq!(result.unwrap().file_name().unwrap(), "agent.leviath");
    }
}
