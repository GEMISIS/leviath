//! Whether a command that needs the daemon may start one, or replace the one
//! that is running.
//!
//! Every command that needs a daemon (`lev run`, `lev serve`, `lev dash`,
//! `lev agent-client`, `lev doctor`, `lev daemon start`) takes `--daemon
//! <attach|ensure|replace>`, and falls back to `LEVIATH_DAEMON` for a host
//! that launches `lev` but cannot add flags to it.

use super::build::Standing;

/// The environment variable read when `--daemon` is not given.
pub const ENV: &str = "LEVIATH_DAEMON";

/// What `--daemon attach` says, and exits non-zero with, when no daemon runs.
pub const NO_DAEMON: &str = "no leviath daemon is running; start one with lev daemon start";

/// How a command treats the daemon it needs.
#[derive(clap::ValueEnum, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DaemonMode {
    /// Use the running daemon, whatever its build. Start none and replace
    /// none: with no daemon running, the command fails.
    Attach,
    /// Start a daemon if none runs, and replace a running one only when it
    /// is strictly older: a lower version, or the same version built from
    /// an earlier commit.
    #[default]
    Ensure,
    /// Start a daemon if none runs, and replace a running one unless it is
    /// the same build or a newer one. A rebuild of the same commit replaces
    /// the daemon it rebuilt.
    Replace,
}

impl DaemonMode {
    /// Whether a command in this mode replaces a running daemon that stands
    /// `standing` to this `lev`.
    pub fn replaces(self, standing: Standing) -> bool {
        matches!(
            (self, standing),
            (Self::Ensure, Standing::Older)
                | (Self::Replace, Standing::Older | Standing::Unordered)
        )
    }

    /// Whether a command in this mode may start a daemon when none runs.
    pub fn starts(self) -> bool {
        self != Self::Attach
    }
}

/// The `--daemon` flag, shared by every command that needs a daemon.
#[derive(clap::Args, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DaemonModeArg {
    /// How to treat the daemon: `attach` uses a running one and never starts
    /// or replaces it; `ensure` (the default) starts one if none runs and
    /// replaces only an older build; `replace` also replaces another build of
    /// the same version, such as a rebuild of one commit. Falls back to
    /// `LEVIATH_DAEMON`.
    #[arg(long = "daemon", value_enum, value_name = "MODE")]
    pub daemon: Option<DaemonMode>,
}

impl DaemonModeArg {
    /// The mode to use: the flag if given, else `env` (the value of
    /// [`ENV`]), else the default. An empty `env` counts as unset; any other
    /// value that names no mode is refused rather than guessed at.
    pub fn resolve(self, env: Option<&str>) -> anyhow::Result<DaemonMode> {
        let env = env.map(str::trim).filter(|value| !value.is_empty());
        match (self.daemon, env) {
            (Some(mode), _) => Ok(mode),
            (None, None) => Ok(DaemonMode::default()),
            (None, Some(value)) => {
                <DaemonMode as clap::ValueEnum>::from_str(value, true).map_err(|_| {
                    anyhow::anyhow!("{ENV} is {value:?}; expected attach, ensure or replace")
                })
            }
        }
    }

    /// [`resolve`](Self::resolve) against this process's environment.
    pub fn resolve_here(self) -> anyhow::Result<DaemonMode> {
        self.resolve(std::env::var(ENV).ok().as_deref())
    }
}

#[cfg(test)]
#[path = "mode_tests.rs"]
mod tests;
