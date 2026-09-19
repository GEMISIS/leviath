//! The `lev serve` command line.
//!
//! In a file of its own rather than beside the response types: these are what
//! an operator writes on a command line, and every one of them is a decision
//! about what the server will and will not do. Keeping them together makes
//! that set readable in one screen, and keeps `types.rs` under its cap.

use std::path::PathBuf;

use clap::Args;

/// Arguments for `lev serve`.
#[derive(Args, Clone)]
pub struct ServeArgs {
    /// Port to listen on
    #[arg(short, long, default_value = "3000")]
    pub port: u16,

    /// Host to bind to
    #[arg(short = 'H', long, default_value = "127.0.0.1")]
    pub host: String,

    /// A name for this server, used for its log file
    /// (`~/.leviath/serve-<NAME>.log`). Defaults to the port, so two servers
    /// side by side keep separate logs. Letters, digits, `.`, `_` and `-`.
    #[arg(long, value_parser = crate::commands::serve::parse_log_name)]
    pub name: Option<String>,

    /// Allow browser requests from this origin (e.g. `http://localhost:5173`).
    ///
    /// Defaults to **none**: the API is for programmatic clients, which are not
    /// subject to CORS at all, so a browser-facing default of `*` gave nothing
    /// to the normal case and widened the surface for the unusual one. A
    /// dashboard served from another origin sets this explicitly.
    ///
    /// `*` is still accepted and still means "any origin". It is now a decision
    /// someone typed rather than what you get by not thinking about it.
    #[arg(long)]
    pub cors: Option<String>,

    /// API token clients must present (`Authorization: Bearer <token>`, or
    /// `?token=` for WebSockets). Overrides the LEVIATH_API_TOKEN env var; the
    /// server refuses to start if neither is set.
    ///
    /// Prefer the environment variable: an argument is visible in `ps` to every
    /// local user for the lifetime of the process.
    #[arg(long)]
    pub token: Option<String>,

    /// Enable the MCP administration endpoints (`POST`/`DELETE
    /// /api/mcp/servers`).
    ///
    /// **Off by default, because they are remote code execution by
    /// construction.** Adding an MCP server writes a `command` and `args` into
    /// `~/.leviath/config.toml`, and Leviath then spawns exactly that - so any
    /// token holder could run an arbitrary process, persistently, for every
    /// future run. The rest of the API can only run agents the user already
    /// installed; this one adds new executables to the machine.
    #[arg(long)]
    pub allow_admin: bool,

    /// Print the GraphQL schema this build serves, then exit.
    ///
    /// The schema is generated from the Rust types, and
    /// `docs/schema/leviath.graphql` is the copy checked in beside the OpenAPI
    /// spec. This is how that copy is regenerated, and a test fails when the
    /// two differ, so the published schema cannot drift from the served one.
    /// Hidden because it serves nothing: it is a build step wearing a flag.
    #[arg(long, hide = true)]
    pub print_graphql_schema: bool,

    /// Restrict agent working directories to this root.
    ///
    /// Without it, `POST /api/agents` accepts any `workdir` - including `/` -
    /// so a token holder can point a tool-executing agent at the whole
    /// filesystem. Set this to the directory the API is meant to work in.
    #[arg(long)]
    pub workdir_root: Option<PathBuf>,

    /// PEM certificate chain to serve HTTPS with. Needs `--tls-key` too.
    ///
    /// Bring your own; Leviath never generates one. Without HTTPS the browser
    /// console cannot reach a `lev serve` that is not on loopback - the browser
    /// blocks the request before sending it, so no server-side header and no
    /// `--cors` value can help. A LAN address is blocked exactly like a public
    /// one.
    ///
    /// `mkcert` and `tailscale cert` both produce certificates that work here.
    /// See the "reaching a Leviath on another machine" section of the docs.
    #[arg(long, value_name = "PATH")]
    pub tls_cert: Option<PathBuf>,

    /// PEM private key for `--tls-cert`. Needs `--tls-cert` too.
    #[arg(long, value_name = "PATH")]
    pub tls_key: Option<PathBuf>,

    /// Refuse `"yolo": true` and `"allow": [...]` on spawn requests, so an API
    /// caller cannot waive approval prompts for an agent running on the host.
    ///
    /// Both fields, because they are one lever: `"allow": ["*"]` reaches the
    /// same wildcard override `"yolo": true` writes.
    #[arg(long)]
    pub no_remote_yolo: bool,

    /// Run every spawn as if it carried `"no_seed_commands": true`, so a
    /// blueprint's `seed = { command = ... }` regions never execute for a
    /// remotely started run.
    ///
    /// A command seed runs at spawn, before the first inference and so before
    /// any approval prompt. `[security] allow_seed_commands = false` refuses
    /// them machine-wide; this refuses them only for runs that arrive over
    /// the API, leaving `lev run` on the host as it was.
    #[arg(long)]
    pub no_remote_seed_commands: bool,

    /// Requests in flight at once before the next one is answered 503.
    ///
    /// Overrides `[serve] max_concurrent_requests` (default 64). `0` disables
    /// the cap. The websocket routes are never counted.
    #[arg(long, value_name = "N")]
    pub max_concurrent_requests: Option<u64>,

    /// Seconds one request may take before it is answered 408.
    ///
    /// Overrides `[serve] request_timeout_secs` (default 30). `0` disables the
    /// timeout. The websocket routes are never timed.
    #[arg(long, value_name = "SECS")]
    pub request_timeout_secs: Option<u64>,
}
