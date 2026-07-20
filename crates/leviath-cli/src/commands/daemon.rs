//! `lev daemon` — run the shared-world daemon in the foreground.
//!
//! The daemon hosts one ECS world for every agent and serves the local control
//! socket. Its execution binds a loopback TCP port, publishes it to the control
//! port file, runs the accept loop, and drives
//! [`leviath_runtime::host::WorldHost::serve`] — real, blocking I/O — so it is
//! routed through [`crate::dispatch::RiskyExecutors`] and implemented by the
//! binary (`main.rs`), not here. This module only defines the command's
//! arguments.

/// Arguments for `lev daemon`.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct DaemonArgs {
    /// Override the control port file (default: `<leviath-home>/.leviath/control.port`).
    #[arg(long)]
    pub socket: Option<String>,
}
