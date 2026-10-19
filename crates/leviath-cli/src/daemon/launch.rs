//! Where a daemon process starts, and what it does not inherit.
//!
//! The daemon is long-lived and shared, and whoever happens to start it first
//! (a terminal inside some project, an editor's `lev agent-client`, a
//! `lev serve` sidecar, launchd) would otherwise hand it their working
//! directory and environment for its whole life. Three things are fixed here:
//!
//! - **The working directory is the data directory.** Nothing the daemon
//!   reads then depends on where it was started: with `load_dotenv` on, the
//!   `.env` it reads is the data directory's. On Windows it also stops the
//!   daemon pinning a project directory, which a running process's current
//!   directory does there.
//! - **Relative path variables are made absolute.** A relative
//!   `LEVIATH_HOME` means "relative to where I ran this", and the daemon no
//!   longer runs there, so the child is handed the absolute path instead.
//! - **What the starter picked up along the way is dropped.** The serve-only
//!   token ([`SERVE_ONLY_ENV`]) and whatever the starter loaded from a
//!   project's `.env` stay with the starter.
//!
//! `PATH` and everything else are inherited on purpose: provider keys exported
//! in a shell are how many people configure Leviath, and a daemon that ignored
//! the starter's `PATH` would lose their toolchains.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Environment variables only `lev serve` reads, removed from every daemon
/// `lev` starts.
///
/// `LEVIATH_API_TOKEN` is the serve API's bearer token. A sidecar that mints a
/// fresh one per window and happens to start the daemon would otherwise leave
/// it in a process that outlives the serve, visible in `ps eww` and to every
/// shell tool run under `shell_env = "inherit"`.
pub const SERVE_ONLY_ENV: &[&str] = &["LEVIATH_API_TOKEN"];

/// Environment variables that name a path the daemon reads or writes. A
/// relative one is resolved against the starter's working directory before
/// the daemon moves to the data directory.
pub const PATH_ENV: &[&str] = &[
    "LEVIATH_HOME",
    "LEVIATH_CONFIG_PATH",
    "LEVIATH_RUNS_DIR",
    "LEVIATH_DUMP_REQUEST_DIR",
];

/// The directory a daemon runs in: the data directory, made absolute against
/// `cwd` and created if this is the first `lev` to run on the machine.
///
/// Takes the resolved data directory (`leviath_core::paths::data_dir()`) so the
/// missing-home case can be told apart from a failure to create it.
pub fn daemon_dir(data_dir: Option<PathBuf>, cwd: &Path) -> anyhow::Result<PathBuf> {
    let dir = data_dir
        .ok_or_else(|| anyhow::anyhow!("cannot resolve a home directory for the daemon"))?;
    let dir = cwd.join(dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// What a daemon `lev` starts in the background inherits differently from
/// its starter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChildEnv {
    /// [`PATH_ENV`] variables the starter has as relative paths, made
    /// absolute against its working directory.
    pub set: Vec<(&'static str, PathBuf)>,
    /// Variables removed: [`SERVE_ONLY_ENV`] and whatever the starter set
    /// from a `.env`.
    pub remove: Vec<String>,
}

impl ChildEnv {
    /// The child environment for a starter whose variables `var` reads, that
    /// runs in `cwd` and set `from_dotenv` from a `.env`
    /// (`config::dotenv_loaded_names`).
    pub fn of(
        var: impl Fn(&str) -> Option<OsString>,
        cwd: &Path,
        from_dotenv: Vec<String>,
    ) -> Self {
        Self {
            set: relative_path_env(&var)
                .into_iter()
                .map(|(name, path)| (name, cwd.join(path)))
                .collect(),
            remove: SERVE_ONLY_ENV
                .iter()
                .map(|name| (*name).to_string())
                .chain(from_dotenv)
                .collect(),
        }
    }
}

/// The [`PATH_ENV`] variables `var` has as relative paths.
fn relative_path_env(var: &impl Fn(&str) -> Option<OsString>) -> Vec<(&'static str, PathBuf)> {
    PATH_ENV
        .iter()
        .filter_map(|name| var(name).map(|value| (*name, PathBuf::from(value))))
        .filter(|(_, path)| path.is_relative())
        .collect()
}

/// Refuse to run the daemon in the foreground with a relative [`PATH_ENV`]
/// variable.
///
/// The foreground daemon moves to the data directory and cannot rewrite its
/// own environment (that needs `unsafe`), so a relative `LEVIATH_HOME` would
/// be read against the wrong directory from then on. A daemon `lev` starts in
/// the background is handed absolute paths instead ([`ChildEnv`]).
pub fn refuse_relative_paths(var: impl Fn(&str) -> Option<OsString>) -> anyhow::Result<()> {
    match relative_path_env(&var).first() {
        None => Ok(()),
        Some((name, path)) => anyhow::bail!(
            "{name} is a relative path ({}); `lev daemon` runs in the data directory, \
             so give it an absolute path",
            path.display()
        ),
    }
}

/// The command that starts a background `lev daemon` from `exe`: in `dir`,
/// with `env` applied and no standard streams.
///
/// The caller detaches and spawns it. Kept apart from the spawn so what the
/// daemon inherits can be checked without starting one.
pub fn daemon_command(exe: &Path, dir: &Path, env: &ChildEnv) -> Command {
    // The daemon is a background process: started from Explorer or a service
    // it has no console to share, so a raw spawn would open one.
    let mut cmd = leviath_sys::child_command(exe);
    cmd.arg("daemon")
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for name in &env.remove {
        cmd.env_remove(name);
    }
    for (name, path) in &env.set {
        cmd.env(name, path);
    }
    cmd
}

#[cfg(test)]
#[path = "launch_tests.rs"]
mod tests;
