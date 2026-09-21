//! The machine this server is on: how it is configured, whether it is healthy,
//! and what is installed on it.
//!
//! These are the answers a settings screen and a diagnostics view need. None of
//! them is a run, and none grows without bound, so they are plain values and
//! plain lists.

use async_graphql::{ID, SimpleObject};

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

/// One MCP server in the config.
#[derive(Debug, SimpleObject)]
pub(crate) struct McpServer {
    /// `mcpServer:<name>`. A machine holds one server per name, so the name is
    /// the whole key.
    #[graphql(owned)]
    pub(crate) id: ID,
    /// Unique server name.
    pub(crate) name: String,
    /// How it is reached.
    pub(crate) transport: McpServerTransport,
    /// The command for a stdio server, the URL for an HTTP one. Empty when the
    /// configuration does not resolve.
    pub(crate) endpoint: String,
    /// Why the configuration does not resolve, when it does not. Null for a
    /// server that does.
    pub(crate) config_error: Option<String>,
    /// Where it stands on credentials.
    pub(crate) auth: McpAuth,
}

impl McpServer {
    /// Describe one server this machine has configured.
    pub(crate) fn from_info(info: super::super::super::mcp::McpServerInfo) -> Self {
        Self {
            id: super::super::node::mcp_server_id(&info.name),
            name: info.name,
            transport: McpServerTransport::from_wire(&info.transport),
            endpoint: info.endpoint,
            config_error: info.config_error,
            auth: McpAuth::from_wire(&info.auth),
        }
    }
}

/// How a configured MCP server on this machine is reached.
///
/// Its own type rather than the manifest's `McpTransport`: a blueprint
/// declares what it wants, and this answers what a configuration on this
/// machine resolved to - including `INVALID`, which no blueprint can declare.
/// `configError` says why it did not resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, async_graphql::Enum)]
pub(crate) enum McpServerTransport {
    /// A command this daemon spawns and talks to over its pipes.
    Stdio,
    /// A URL this daemon calls.
    Http,
    /// Neither: the configuration did not resolve.
    Invalid,
}

impl McpServerTransport {
    /// Read the word the server description carries.
    ///
    /// Anything else is `INVALID`, which is what a word this build does not
    /// know amounts to: a transport it cannot use.
    pub(crate) fn from_wire(word: &str) -> Self {
        match word {
            "stdio" => Self::Stdio,
            "http" => Self::Http,
            _ => Self::Invalid,
        }
    }
}

/// Where a server stands on credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, async_graphql::Enum)]
pub(crate) enum McpAuth {
    /// A stdio server, which has nobody to log in to.
    NotApplicable,
    /// An HTTP server with no credential of any kind.
    None,
    /// An `Authorization` header from the config. A credential, so no login is
    /// offered for it.
    Header,
    /// Signed in, and the token has not expired.
    Authenticated,
    /// Signed in once; the token has expired and the server needs another.
    Expired,
}

impl McpAuth {
    /// Read the word the server description carries.
    ///
    /// An unknown word reads as `NONE`: the safe reading, since it is the one
    /// that offers a login rather than assuming a credential is in place.
    pub(crate) fn from_wire(word: &str) -> Self {
        match word {
            "n/a" => Self::NotApplicable,
            "header" => Self::Header,
            "authenticated" => Self::Authenticated,
            "expired" => Self::Expired,
            _ => Self::None,
        }
    }
}

/// One named yolo profile, summarised.
///
/// The counts rather than the rules: a settings list shows how much a profile
/// waives, and `lev yolo show` prints the rules themselves.
#[derive(Debug, SimpleObject)]
pub(crate) struct YoloProfile {
    /// `yoloProfile:<name>`. One file holds the profiles, one profile per
    /// name, so the name is the whole key.
    #[graphql(owned)]
    pub(crate) id: ID,
    /// The profile's name, as `--yolo=<name>` spells it.
    pub(crate) name: String,
    /// What tools with no explicit rule do.
    pub(crate) default: YoloWaiver,
    /// What happens to the run's own questions.
    pub(crate) questions: YoloHuman,
    /// What happens at blueprint checkpoints.
    pub(crate) checkpoints: YoloHuman,
    /// What happens at the taint gate.
    pub(crate) gate: YoloHuman,
    /// How many tool rules it carries: allow, ask, deny.
    pub(crate) tool_rules: Vec<i32>,
    /// How many shell rules it carries: allow, ask, deny.
    pub(crate) shell_rules: Vec<i32>,
}

/// What a profile does with a tool no rule names.
///
/// Two values, not three: a profile waives prompts, it never adds a refusal.
/// A tool a profile does not reach is decided by the config's own permissions,
/// which can still deny it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, async_graphql::Enum)]
pub(crate) enum YoloWaiver {
    /// Runs without asking.
    Allow,
    /// Stops and asks, as it would with no profile.
    Ask,
}

/// Whether one human-in-the-loop mechanism still reaches a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, async_graphql::Enum)]
pub(crate) enum YoloHuman {
    /// Reaches a person and waits.
    Ask,
    /// Answers itself and carries on.
    Auto,
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
    pub(crate) is_text: Option<bool>,
    /// The extensions this type is known by.
    pub(crate) extensions: Vec<String>,
}

/// One registered script.
#[derive(Debug, SimpleObject)]
pub(crate) struct Script {
    /// `script:<kind>:<name>` for a script every blueprint gets, and
    /// `script:<kind>@<blueprint>:<name>` for one blueprint's own.
    ///
    /// The kind and the owning blueprint as well as the name, because a name is
    /// unique only within its kind and the directory it came from: one machine
    /// can hold a global `tool` called `summarise` and a blueprint's own `tool`
    /// of that name, and they are two scripts.
    #[graphql(owned)]
    pub(crate) id: ID,
    /// Which registry it belongs to: a tool, a hook, a validator, a mime check
    /// or a provider.
    pub(crate) kind: String,
    /// Its name, unique within that kind and the directory it came from.
    pub(crate) name: String,
    /// Where it was found: the directory kind this script was read from.
    pub(crate) found_at: String,
    /// The blueprint whose directory it came from, for a blueprint-scoped
    /// script.
    pub(crate) blueprint: Option<String>,
}

impl Script {
    /// Describe one script this machine has registered.
    pub(crate) fn from_item(item: super::super::super::scripts::ScriptItem) -> Self {
        Self {
            id: super::super::node::script_id(&item.kind, item.agent.as_deref(), &item.name),
            kind: item.kind,
            name: item.name,
            found_at: item.source,
            blueprint: item.agent,
        }
    }
}

/// One write the daemon attempted and lost.
#[derive(Debug, SimpleObject)]
pub(crate) struct JournalWriteError {
    /// The run whose write it was.
    pub(crate) run_id: String,
    /// The file that could not be written, as the daemon resolved it.
    pub(crate) path: String,
    /// What the operating system said about it.
    pub(crate) message: String,
    /// When it happened.
    pub(crate) at: Timestamp,
}

/// What the daemon has recorded about its runs, and what it has lost.
///
/// A daemon that cannot write a run's journal answers every request and reports
/// every lane as idle, so this is the only field that says so. A run whose
/// journal record cannot be written is failed, rather than carried on with a
/// history that cannot record what it did.
#[derive(Debug, SimpleObject)]
pub(crate) struct JournalHealth {
    /// Whether every write the daemon has attempted reached the disk. Once
    /// something has been lost this stays false for the life of the daemon: a
    /// record that went missing does not come back.
    pub(crate) healthy: bool,
    /// Journal records the daemon has tried to append.
    pub(crate) appends_attempted: BigInt,
    /// Those it could not write, retry included. Each one is something a run did
    /// that its journal does not mention.
    pub(crate) appends_failed: BigInt,
    /// Snapshot writes that lost at least one file. A snapshot is rewritten
    /// whole whenever the run changes, so these cost freshness rather than
    /// history.
    pub(crate) snapshots_failed: BigInt,
    /// How many messages the persistence lane took in one go the last time it
    /// looked. A large number means runs are queueing behind the disk.
    pub(crate) queue_depth: i32,
    /// The most recent write the daemon lost, with the run and the file. Null
    /// while nothing has been lost.
    pub(crate) last_error: Option<JournalWriteError>,
}

impl JournalHealth {
    /// The daemon's reading, as the schema says it.
    pub(crate) fn of(health: &leviath_runtime::persist_stats::JournalHealth) -> Self {
        Self {
            healthy: health.is_healthy(),
            appends_attempted: BigInt(i64::try_from(health.appends_attempted).unwrap_or(i64::MAX)),
            appends_failed: BigInt(i64::try_from(health.appends_failed).unwrap_or(i64::MAX)),
            snapshots_failed: BigInt(i64::try_from(health.snapshots_failed).unwrap_or(i64::MAX)),
            queue_depth: i32::try_from(health.queue_depth).unwrap_or(i32::MAX),
            last_error: health.last_error.as_ref().map(|error| JournalWriteError {
                run_id: error.run_id.clone(),
                path: error.path.clone(),
                message: error.message.clone(),
                at: Timestamp(error.at),
            }),
        }
    }
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

#[cfg(test)]
#[path = "machine_tests.rs"]
mod tests;
