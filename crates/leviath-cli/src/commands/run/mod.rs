//! `lev run` — run an agent in the shared-world daemon.
//!
//! `run` resolves the blueprint + task locally and asks the running daemon (auto-
//! started if needed) to create the agent in the one shared ECS world. The
//! request-building + daemon exchange live in [`crate::daemon::client`]; this
//! module keeps the manifest/session/tool-source helpers still shared across the
//! CLI, and the `RunArgs` the binary wires into that path.

pub mod manifest;
pub mod session;

use clap::Args;

// Re-export the provider-registry builders used by the daemon setup.
pub use session::{
    ProviderCreds, build_provider_registry, build_provider_registry_from_config,
    provider_creds_from_config,
};

/// Arguments for `lev run`.
#[derive(Args, Debug, Clone, Default)]
pub struct RunArgs {
    /// Path to the agent (a manifest file, its directory, or an installed name).
    #[arg(value_name = "PATH")]
    pub path: Option<String>,

    /// Task prompt for the agent.
    #[arg(short, long)]
    pub task: Option<String>,

    /// Model override (`provider/model` or a bare model name).
    #[arg(short, long)]
    pub model: Option<String>,

    /// Approve every tool call without prompting.
    #[arg(long)]
    pub yolo: bool,

    /// Allow a tool outright (repeatable).
    #[arg(long)]
    pub allow: Vec<String>,

    /// Override the blueprint's max sub-agent tree depth.
    #[arg(long)]
    pub max_depth: Option<usize>,
}
