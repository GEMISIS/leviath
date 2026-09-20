//! The machine this server is on: how it is configured, whether it is healthy,
//! and what is installed on it.
//!
//! These are the answers a settings screen and a diagnostics view need. None of
//! them is a run, and none grows without bound, so they are plain values and
//! plain lists.

use async_graphql::SimpleObject;

use super::super::scalars::{BigInt, Timestamp};

/// What the server will and will not do, in numbers.
///
/// Published so a client never hardcodes a cap. Reading the limit is how a
/// paging loop knows what to ask for, rather than discovering it by being
/// refused.
#[derive(Debug, SimpleObject)]
pub(crate) struct ServeLimits {
    /// The largest page any listing will serve.
    pub(crate) max_page_size: i32,
    /// The most ids one batch fetch may name.
    pub(crate) max_ids: i32,
    /// The most bytes one file read returns.
    pub(crate) max_file_bytes: BigInt,
    /// The most entries one directory listing returns.
    pub(crate) max_listing_entries: i32,
    /// How many runs a file-reading search examines before giving up.
    pub(crate) max_search_scan: i32,
    /// The largest page of context history.
    pub(crate) max_history_limit: i32,
    /// The most requests served at once.
    pub(crate) max_concurrent_requests: BigInt,
    /// The largest accepted upload, in bytes.
    pub(crate) max_upload_bytes: BigInt,
    /// The per-request deadline, in seconds.
    pub(crate) request_timeout_secs: i32,
}

/// Why the config file does not load.
#[derive(Debug, SimpleObject)]
pub(crate) struct ConfigError {
    /// Which step refused it: reading, parsing, or validating.
    pub(crate) kind: String,
    /// The file, as the server resolved it.
    pub(crate) path: String,
    /// One line on what is wrong.
    pub(crate) message: String,
    /// The line a parse failure is at, 1-based.
    pub(crate) line: Option<i32>,
    /// The column beside it.
    pub(crate) column: Option<i32>,
    /// The dotted config key a validation failure is about.
    pub(crate) key: Option<String>,
    /// Said in words, for a client that only renders strings.
    pub(crate) note: String,
}

/// A custom model gateway from `[model_providers]`.
#[derive(Debug, SimpleObject)]
pub(crate) struct Gateway {
    /// The name an agent references, and the table key.
    pub(crate) name: String,
    /// Where the gateway lives, when the entry names one.
    pub(crate) base_url: Option<String>,
    /// Whether a key is configured. The value itself never crosses the wire.
    pub(crate) has_api_key: bool,
    /// What backs it: a script, or an endpoint.
    pub(crate) kind: String,
}

/// The daemon's configuration, with every secret left out.
///
/// The `has*Key` flags are the shape this takes deliberately: a console needs
/// to know whether a provider is configured, and never needs the key.
#[derive(Debug, SimpleObject)]
pub(crate) struct Config {
    /// The default provider, by name.
    pub(crate) default_provider: String,
    /// Providers allowed to serve a bare model name, best first. Empty means
    /// the default provider alone decides.
    pub(crate) provider_order: Vec<String>,
    /// The model every stage that permits it starts on, ahead of its own list.
    pub(crate) override_model: Option<String>,
    /// The model tried after every model a stage names.
    pub(crate) fallback_model: Option<String>,
    /// Whether a key is stored for each provider. Names only; no values.
    pub(crate) configured_providers: Vec<String>,
    /// Custom model gateways.
    pub(crate) gateways: Vec<Gateway>,
    /// Where this server looks for blueprints, beyond the installed agents
    /// directory.
    pub(crate) agent_paths: Vec<String>,
    /// How many MCP servers the config declares.
    pub(crate) mcp_server_count: i32,
    /// The API version this server speaks.
    pub(crate) api_version: String,
    /// What this server can do. Check these rather than calling a route and
    /// reading a 404, which also means "no such run".
    pub(crate) capabilities: Vec<String>,
    /// Whether this server was started with `--allow-admin`, so the mutations
    /// that change the machine will run rather than answer `FORBIDDEN`. Worth
    /// asking before offering a settings screen that cannot save.
    pub(crate) admin_enabled: bool,
    /// The numbers above.
    pub(crate) limits: ServeLimits,
    /// Why the config file does not load. Absent when it does.
    pub(crate) config_error: Option<ConfigError>,
    /// When the config was last saved. While unhealthy this is the last good
    /// save, not what is on disk.
    pub(crate) config_mtime: Option<Timestamp>,
}

/// One environment or configuration check.
#[derive(Debug, SimpleObject)]
pub(crate) struct DoctorCheck {
    /// What was checked.
    pub(crate) name: String,
    /// Whether it passed.
    pub(crate) ok: bool,
    /// What was found, or why not.
    pub(crate) detail: String,
}

/// The diagnostics report.
#[derive(Debug, SimpleObject)]
pub(crate) struct DoctorReport {
    /// Whether every check passed.
    pub(crate) ok: bool,
    /// The checks themselves, in the order they ran.
    pub(crate) checks: Vec<DoctorCheck>,
}

/// One MCP server in the config.
#[derive(Debug, SimpleObject)]
pub(crate) struct McpServer {
    /// Unique server name.
    pub(crate) name: String,
    /// How it is reached: `stdio`, `http`, or `invalid` when the configured
    /// transport did not resolve.
    pub(crate) transport: String,
    /// The command for a stdio server, the URL for an HTTP one. Empty when the
    /// transport is invalid.
    pub(crate) endpoint: String,
    /// Its one-word auth state.
    pub(crate) auth: String,
}

/// One named yolo profile, summarised.
///
/// The counts rather than the rules: a settings list shows how much a profile
/// waives, and `lev yolo show` prints the rules themselves.
#[derive(Debug, SimpleObject)]
pub(crate) struct YoloProfile {
    /// The profile's name, as `--yolo=<name>` spells it.
    pub(crate) name: String,
    /// What tools with no explicit rule do.
    pub(crate) default: String,
    /// What happens to the agent's own questions.
    pub(crate) questions: String,
    /// What happens at blueprint checkpoints.
    pub(crate) checkpoints: String,
    /// What happens at the taint gate.
    pub(crate) gate: String,
    /// How many tool rules it carries: allow, ask, deny.
    pub(crate) tool_rules: Vec<i32>,
    /// How many shell rules it carries: allow, ask, deny.
    pub(crate) shell_rules: Vec<i32>,
}

/// The yolo profiles, and where they are read from.
#[derive(Debug, SimpleObject)]
pub(crate) struct YoloProfiles {
    /// The file the profiles are read from.
    pub(crate) path: String,
    /// Whether that file exists. False with no error means `--yolo=<name>` has
    /// nothing to name yet.
    pub(crate) exists: bool,
    /// Why the file does not load, when it does not. The profiles are then
    /// empty, and a spawn naming one is refused with this same message.
    pub(crate) error: Option<String>,
    /// The profiles themselves.
    pub(crate) profiles: Vec<YoloProfile>,
}

/// One row of the mime registry.
#[derive(Debug, SimpleObject)]
pub(crate) struct MimeRow {
    /// The row's key: a type, or a pattern such as `image/*`.
    pub(crate) mime_type: String,
    /// Where the row came from: `builtin`, `config`, or a blueprint's name.
    pub(crate) source: String,
    /// The family the type resolves to, which is what providers key their
    /// encoders on.
    pub(crate) family: Option<String>,
    /// Whether the bytes are text, and so may travel inline.
    pub(crate) text: Option<bool>,
    /// The extensions this type is known by.
    pub(crate) extensions: Vec<String>,
}

/// One registered script.
#[derive(Debug, SimpleObject)]
pub(crate) struct Script {
    /// Which registry it belongs to: a tool, a hook, a validator, a mime check
    /// or a provider.
    pub(crate) kind: String,
    /// Its name, unique within that kind.
    pub(crate) name: String,
    /// Where it was found.
    pub(crate) source: String,
    /// The agent whose directory it came from, for an agent-scoped script.
    pub(crate) agent: Option<String>,
}

/// One directory, for a file picker.
#[derive(Debug, SimpleObject)]
pub(crate) struct Directory {
    /// The absolute directory that was listed.
    pub(crate) path: String,
    /// Where "up one level" goes. Null at the filesystem root, and at the
    /// workdir root: a picker is never led above the fence.
    pub(crate) parent: Option<String>,
    /// The user's home directory, for a "home" shortcut.
    pub(crate) home: String,
    /// This server's own working directory, for a "here" shortcut.
    pub(crate) cwd: String,
    /// The directories inside, by name.
    pub(crate) entries: Vec<String>,
}
