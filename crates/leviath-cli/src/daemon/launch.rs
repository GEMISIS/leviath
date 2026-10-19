//! Where a daemon process starts, and what it does not inherit.
//!
//! The daemon is long-lived and shared, and whoever happens to start it first
//! (a terminal inside some project, an editor's `lev agent-client`, a
//! `lev serve` sidecar, launchd) would otherwise hand it their working
//! directory and environment for its whole life. Two things are fixed here:
//!
//! - **The working directory is the data directory.** Nothing the daemon
//!   reads then depends on where it was started: with `load_dotenv` on, the
//!   `.env` it reads is the data directory's. On Windows it also stops the
//!   daemon pinning a project directory, which a running process's current
//!   directory does there.
//! - **Serve-only credentials are dropped.** [`SERVE_ONLY_ENV`] names what
//!   only `lev serve` reads; a daemon that serve starts never sees it.
//!
//! `PATH` and everything else are inherited on purpose: provider keys exported
//! in a shell are how many people configure Leviath, and a daemon that ignored
//! the starter's `PATH` would lose their toolchains.

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

/// The directory a daemon runs in: the data directory, created if this is the
/// first `lev` to run on the machine.
///
/// Takes the resolved data directory (`leviath_core::paths::data_dir()`) so the
/// missing-home case can be told apart from a failure to create it.
pub fn daemon_dir(data_dir: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let dir = data_dir
        .ok_or_else(|| anyhow::anyhow!("cannot resolve a home directory for the daemon"))?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The command that starts a background `lev daemon` from `exe`: in `dir`,
/// with no serve-only credential and no standard streams.
///
/// The caller detaches and spawns it. Kept apart from the spawn so what the
/// daemon inherits can be checked without starting one.
pub fn daemon_command(exe: &Path, dir: &Path) -> Command {
    // The daemon is a background process: started from Explorer or a service
    // it has no console to share, so a raw spawn would open one.
    let mut cmd = leviath_sys::child_command(exe);
    cmd.arg("daemon")
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    without_serve_env(&mut cmd);
    cmd
}

/// Remove every [`SERVE_ONLY_ENV`] variable from what `cmd`'s child inherits.
pub fn without_serve_env(cmd: &mut Command) -> &mut Command {
    for name in SERVE_ONLY_ENV {
        cmd.env_remove(name);
    }
    cmd
}

#[cfg(test)]
#[path = "launch_tests.rs"]
mod tests;
