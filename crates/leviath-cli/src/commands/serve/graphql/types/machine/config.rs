//! The daemon's own configuration and the diagnostics that read it: the
//! numeric limits a client should never hardcode, why the config file does not
//! load when it does not, and the environment checks `doctor` reports.

use async_graphql::SimpleObject;

use super::super::super::scalars::{BigInt, Timestamp};

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
    /// When this server first saw the file in this state. A banner that has
    /// been up for an hour is a different thing from one that appeared while
    /// somebody was editing.
    pub(crate) since: Timestamp,
    /// Said in words, for a client that only renders strings.
    pub(crate) note: String,
}

/// A custom model gateway from `[model_providers]`.
#[derive(Debug, SimpleObject)]
pub(crate) struct Gateway {
    /// The name a blueprint references, and the table key.
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
