//! Finding a blueprint on disk: the `agent.toml` an agent argument names.
//!
//! Reading one is [`leviath_blueprint::load`]; this only decides which file an
//! argument means, by one rule for every command that takes a name or a path.

use std::path::{Path, PathBuf};

use leviath_blueprint::FILE_NAME;
use leviath_runtime::spec::env::LoadedBlueprint;

/// The blueprint at `path` (an `agent.toml`, or the directory holding one),
/// or `None` when it cannot be read.
///
/// For the warnings a spawn prints beside the daemon's answer: a blueprint
/// that will not read is the daemon's to report as the spawn error, and a
/// warning must never be why a spawn fails.
pub(crate) fn loaded_at(path: &Path) -> Option<LoadedBlueprint> {
    leviath_blueprint::load(path).ok()
}

/// Resolve an agent argument to the `agent.toml` it names.
///
/// Accepts the file itself, a directory containing one, or an installed
/// agent's name, and falls back to the blueprint in the current directory.
/// The installed tree is the shared `LEVIATH_HOME`-aware one, so `lev run
/// <name>` finds what `lev add` wrote when the override is set.
pub(crate) fn find_blueprint(path: &str) -> anyhow::Result<PathBuf> {
    find_blueprint_in(
        path,
        leviath_core::paths::agents_dir().as_deref(),
        Path::new(""),
    )
}

/// [`find_blueprint`] against a given installed-agents directory and a given
/// current directory, so every command that takes an agent name-or-path
/// resolves it by one rule and a test can point both somewhere of its own.
/// `cwd` may be empty, which leaves the fallback path relative as typed.
pub(crate) fn find_blueprint_in(
    path: &str,
    agents_dir: Option<&Path>,
    cwd: &Path,
) -> anyhow::Result<PathBuf> {
    let p = Path::new(path);
    // 1. The file itself.
    if p.is_file() && p.file_name() == Some(std::ffi::OsStr::new(FILE_NAME)) {
        return Ok(p.to_path_buf());
    }
    // 2. A directory holding one, 3. an installed agent by name, or 4. the
    // current directory's.
    let candidates = [
        p.is_dir().then(|| p.join(FILE_NAME)),
        agents_dir.map(|d| d.join(path).join(FILE_NAME)),
        Some(cwd.join(FILE_NAME)),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|c| c.is_file())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Could not find a blueprint for '{path}'. Pass a path to a directory \
                 containing {FILE_NAME}, or an installed agent name (see `lev list`)."
            )
        })
}

#[cfg(test)]
#[path = "locate_tests.rs"]
mod tests;
