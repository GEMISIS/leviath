//! `lev daemon status --json`: what is running, which `lev` is asking, and
//! what a command that needs the daemon would do about it.
//!
//! The field names are a stable interface for programs that embed `lev`.
//! Fields are only ever added. Nothing secret goes in: no token, and no
//! environment values.

use serde::Serialize;

use super::build::{Build, Standing};
use super::mode::DaemonMode;

/// What `lev daemon status` observed, handed over by the binary, which owns
/// the socket, the files and the process.
#[derive(Debug, Clone, Default)]
pub struct Observed {
    /// Whether a daemon answers on the control socket.
    pub running: bool,
    /// The daemon's build marker, `None` when it wrote none.
    pub marker: Option<String>,
    /// Who the daemon said it is in its handshake, `None` when it did not.
    pub identity: Option<leviath_runtime::control_socket::DaemonIdentity>,
    /// How many runs the daemon is hosting.
    pub runs: usize,
    /// Whether the supervisor's service file is installed, and where it
    /// goes. `None` on a platform with no supported supervisor.
    pub supervised: Option<(bool, std::path::PathBuf)>,
    /// The path of the `lev` executable asking.
    pub cli_exe: Option<String>,
}

/// The object `lev daemon status --json` prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusReport {
    /// Whether a daemon answers on the control socket.
    pub running: bool,
    /// The daemon's process id, from its handshake.
    pub pid: Option<u32>,
    /// The daemon's executable, from its handshake. Null from a daemon too
    /// old to report it.
    pub exe: Option<String>,
    /// The daemon's version, from its build marker.
    pub version: Option<String>,
    /// The daemon's build id, from its build marker.
    pub build: Option<String>,
    /// When the commit the daemon was built from was made, in unix seconds.
    pub committed_at: Option<u64>,
    /// How many runs the daemon is hosting.
    pub runs: usize,
    /// Whether the daemon is registered with the OS supervisor.
    pub supervised: Option<Supervised>,
    /// The `lev` asking.
    pub cli: CliReport,
    /// How the daemon's build stands to this `lev`'s. Null when no daemon
    /// runs.
    pub standing: Option<Standing>,
    /// Whether a command that needs the daemon, run from this `lev` in the
    /// default `--daemon` mode, would replace the running one.
    pub spawn_would_replace: bool,
}

/// Whether the daemon is registered with the OS supervisor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Supervised {
    /// Whether the service file is installed.
    pub installed: bool,
    /// Where the service file goes.
    pub path: String,
}

/// The `lev` asking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CliReport {
    /// Its executable.
    pub exe: Option<String>,
    /// Its version.
    pub version: Option<String>,
    /// Its build id.
    pub build: String,
    /// When the commit it was built from was made, in unix seconds.
    pub committed_at: Option<u64>,
}

/// Build the report from what was observed, for the `lev` built as `cli`.
pub fn report(observed: Observed, cli: &Build) -> StatusReport {
    let running = observed.running;
    let daemon = observed
        .marker
        .as_deref()
        .map(Build::parse)
        .filter(|_| running);
    let identity = observed.identity.filter(|_| running);
    let standing =
        running.then(|| Build::parse(observed.marker.as_deref().unwrap_or_default()).standing(cli));
    StatusReport {
        running,
        pid: identity.as_ref().map(|identity| identity.pid),
        exe: identity.and_then(|identity| identity.exe),
        version: daemon.as_ref().and_then(|daemon| daemon.version.clone()),
        build: daemon.as_ref().map(|daemon| daemon.id.clone()),
        committed_at: daemon.and_then(|daemon| daemon.committed_at),
        runs: observed.runs,
        supervised: observed.supervised.map(|(installed, path)| Supervised {
            installed,
            path: path.display().to_string(),
        }),
        cli: CliReport {
            exe: observed.cli_exe,
            version: cli.version.clone(),
            build: cli.id.clone(),
            committed_at: cli.committed_at,
        },
        standing,
        spawn_would_replace: standing
            .is_some_and(|standing| DaemonMode::default().replaces(standing)),
    }
}

/// [`report`] as the pretty-printed JSON `lev daemon status --json` prints.
pub fn to_json(report: &StatusReport) -> String {
    serde_json::to_string_pretty(report).expect("a status report serializes")
}

#[cfg(test)]
#[path = "status_report_tests.rs"]
mod tests;
