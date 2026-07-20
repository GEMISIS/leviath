//! `lev daemon` — run the shared-world daemon in the foreground.
//!
//! The daemon hosts one ECS world for every agent and serves the local control
//! socket. Its execution binds a Unix socket, runs the accept loop, and drives
//! [`leviath_runtime::host::WorldHost::serve`] — real, blocking I/O — so it is
//! routed through [`crate::dispatch::RiskyExecutors`] and implemented by the
//! binary (`main.rs`), not here. This module only defines the command's
//! arguments.

/// Arguments for `lev daemon`.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct DaemonArgs {
    /// Override the control-socket path (default: `<leviath-home>/.leviath/control.sock`).
    #[arg(long)]
    pub socket: Option<String>,
}
