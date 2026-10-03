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
    let agents_dir = leviath_core::paths::agents_dir();
    find_blueprint_in(path, agents_dir.as_deref(), Path::new(""))
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
            // A directory, or an install, holding only the manifest an earlier
            // release wrote is one command from runnable, so say which.
            let old = [Some(p.to_path_buf()), agents_dir.map(|d| d.join(path))]
                .into_iter()
                .flatten()
                .find_map(|at| old_format(&at, path, agents_dir));
            match old {
                Some(message) => anyhow::anyhow!(message),
                None => anyhow::anyhow!(
                    "Could not find a blueprint for '{path}'. Pass a path to a directory \
                     containing {FILE_NAME}, or an installed agent name (see `lev list`)."
                ),
            }
        })
}

/// The manifest file an earlier release wrote, in place of [`FILE_NAME`].
pub(crate) const OLD_FILE_NAME: &str = "agent.leviath";

/// What to say when `path` is a blueprint an earlier release wrote: an
/// `agent.leviath` file, or a directory holding one and no [`FILE_NAME`].
/// `None` for anything else. `shown` is the argument as the person typed it;
/// a blueprint installed in `agents_dir` is upgraded by the daemon, which the
/// message says too.
///
/// Every command that takes a blueprint by path says this, so an old file is
/// never reported as "not found" or "not a blueprint" when one command would
/// make it work.
pub(crate) fn old_format(path: &Path, shown: &str, agents_dir: Option<&Path>) -> Option<String> {
    let dir = match path.file_name() == Some(std::ffi::OsStr::new(OLD_FILE_NAME)) {
        true => path.parent().unwrap_or(path),
        false => path,
    };
    let old = dir.join(OLD_FILE_NAME).is_file() && !dir.join(FILE_NAME).is_file();
    old.then(|| {
        let installed = match dir.parent().is_some_and(|p| Some(p) == agents_dir) {
            true => {
                " The daemon upgrades installed blueprints when it starts (`lev daemon restart`)."
            }
            false => "",
        };
        format!(
            "'{shown}' is an {OLD_FILE_NAME} from an earlier release, at {}, and this release \
             reads {FILE_NAME}: convert it with `lev blueprint migrate {} -o {}`.{installed}",
            dir.display(),
            dir.display(),
            dir.join(FILE_NAME).display()
        )
    })
}

#[cfg(test)]
#[path = "locate_tests.rs"]
mod tests;
