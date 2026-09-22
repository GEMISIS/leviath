//! The daemon's own configuration and the diagnostics that read it: the
//! numeric limits a client should never hardcode, every provider this build
//! knows and how it is set up, why the config file does not load when it does
//! not, and the environment checks `doctor` reports.
//!
//! Everything a config write can set is read back here under the same name and
//! the same nesting, so a settings screen renders what it saves. A secret is
//! the one exception and always will be: a key reads back as `hasKey`.

use async_graphql::{Enum, ID, SimpleObject, Union};
use leviath_graphql_derive::mirror;

use super::super::super::scalars::{BigInt, Timestamp};

/// What the server will and will not do, in numbers.
///
/// Published so a client never hardcodes a cap. Reading the limit is how a
/// paging loop knows what to ask for, rather than discovering it by being
/// refused.
#[mirror]
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

/// Which step refused the config file.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum ConfigErrorKind {
    /// The file is there and could not be read.
    Read,
    /// The bytes are not TOML this build can parse, or a value has the wrong
    /// type for the field it was given to.
    Parse,
    /// It parsed, and then one of its values was refused: an endpoint with no
    /// address, an MCP server with no command.
    Validate,
    /// A word this build does not know, from a daemon newer than it.
    Unknown,
}

impl ConfigErrorKind {
    /// Read the word the health snapshot carries.
    pub(crate) fn from_wire(word: &str) -> Self {
        match word {
            "read" => Self::Read,
            "parse" => Self::Parse,
            "validation" => Self::Validate,
            _ => Self::Unknown,
        }
    }
}

/// Why the config file does not load.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct ConfigError {
    /// Which step refused it: reading, parsing, or validating.
    pub(crate) kind: ConfigErrorKind,
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

/// Whether the config being served is the file on disk.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct ConfigHealth {
    /// Why the file does not load. Null while it does, and while it is null
    /// every other field describes the file on disk.
    pub(crate) error: Option<ConfigError>,
    /// When the config in force was last saved. While `error` is set this is
    /// the last good save rather than what is on disk, which is how a client
    /// that has just written confirms the write was picked up.
    pub(crate) saved_at: Option<Timestamp>,
}

/// One file this server reads beside the config, and whether it is readable.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct ConfigFileStatus {
    /// Where the file is, as this server resolved it.
    pub(crate) path: String,
    /// Whether there is one there. A file that is not there is not a fault:
    /// it means the defaults.
    pub(crate) exists: bool,
    /// Why it does not load, when it does not.
    pub(crate) error: Option<String>,
}

/// What backs a custom gateway.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum GatewayKind {
    /// A Rhai provider script in the providers directory.
    Script,
    /// A server speaking OpenAI's chat API, reached natively with no script.
    OpenaiCompatible,
    /// OpenAI's own API at another host, under a name of its own.
    Openai,
}

impl GatewayKind {
    /// Read the word the config file uses.
    ///
    /// A word this build does not know reads as `SCRIPT`, which is the kind
    /// an entry with no `kind` at all gets in the file itself.
    pub(crate) fn from_wire(word: &str) -> Self {
        match word {
            "openai-compatible" => Self::OpenaiCompatible,
            "openai" => Self::Openai,
            _ => Self::Script,
        }
    }

    /// The spelling the config file uses.
    pub(crate) fn as_wire(self) -> &'static str {
        match self {
            Self::Script => "script",
            Self::OpenaiCompatible => "openai-compatible",
            Self::Openai => "openai",
        }
    }
}

/// A custom model gateway from `[model_providers]`.
#[mirror(list)]
#[derive(Debug, SimpleObject)]
pub(crate) struct Gateway {
    /// The name a blueprint references, and the table key.
    pub(crate) name: String,
    /// What backs it: a script, or an endpoint.
    pub(crate) kind: GatewayKind,
    /// Where the gateway lives, when the entry names one.
    pub(crate) base_url: Option<String>,
    /// The Rhai provider script behind it, when the entry names one.
    pub(crate) script: Option<String>,
    /// Whether a key is configured. The value itself never crosses the wire.
    pub(crate) has_api_key: bool,
    /// The names of the extra headers an endpoint sends, without their values:
    /// a header is where a second credential goes.
    pub(crate) header_names: Vec<String>,
    /// The model ids an endpoint falls back to when its server will not list
    /// them.
    pub(crate) models: Vec<String>,
    /// The names of any other keys the entry carries, without their values.
    /// They are handed to the script verbatim, so they routinely hold
    /// credentials.
    pub(crate) unknown_keys: Vec<String>,
}

/// How a provider proves who this machine is.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum ProviderAuthKind {
    /// An API key stored in the config.
    ApiKey,
    /// A browser sign-in against a subscription, taken by `signInProvider`.
    SignIn,
    /// Neither: a server on this machine or one that asks for nothing.
    None,
}

/// How hard the Codex transport thinks before it answers.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum CodexReasoningEffort {
    /// No reasoning pass at all.
    None,
    /// The shortest pass the provider offers.
    Minimal,
    /// A short pass.
    Low,
    /// The provider's own default.
    Medium,
    /// A long pass.
    High,
    /// The longest pass the provider offers.
    Xhigh,
}

impl CodexReasoningEffort {
    /// The word the config file uses.
    pub(crate) fn as_wire(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }

    /// Read the config file's word. `None` for a word this build does not
    /// know, which is what a hand-edited file can hold: the provider ignores
    /// such a value, and reporting it as one of these would claim otherwise.
    pub(crate) fn from_wire(word: &str) -> Option<Self> {
        [
            Self::None,
            Self::Minimal,
            Self::Low,
            Self::Medium,
            Self::High,
            Self::Xhigh,
        ]
        .into_iter()
        .find(|effort| effort.as_wire() == word)
    }
}

/// How much the Codex transport writes.
#[mirror]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub(crate) enum CodexVerbosity {
    /// The least it will say.
    Low,
    /// The provider's own default.
    Medium,
    /// The most it will say.
    High,
}

impl CodexVerbosity {
    /// The word the config file uses.
    pub(crate) fn as_wire(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// Read the config file's word, with the same reading of an unknown one as
    /// [`CodexReasoningEffort::from_wire`].
    pub(crate) fn from_wire(word: &str) -> Option<Self> {
        [Self::Low, Self::Medium, Self::High]
            .into_iter()
            .find(|verbosity| verbosity.as_wire() == word)
    }
}

/// The settings the Codex transport has and no other provider does.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct CodexOptions {
    /// How hard it thinks. Null when the config pins nothing, which leaves the
    /// provider's own default.
    pub(crate) reasoning_effort: Option<CodexReasoningEffort>,
    /// How much it writes, with the same null.
    pub(crate) verbosity: Option<CodexVerbosity>,
    /// Whether a turn's opaque reasoning token is sent back on the next one.
    pub(crate) replays_reasoning: bool,
}

/// The settings one provider has that the others do not.
///
/// A union rather than a bag of nullable fields on every provider: only Codex
/// has any of these, and a field that is null on nine providers out of ten
/// reads as a setting nobody has rather than as one that does not apply.
#[mirror]
#[derive(Debug, Union)]
pub(crate) enum ProviderOptions {
    /// The Codex transport's own settings.
    Codex(CodexOptions),
}

/// One provider this build knows, as this machine has it set up.
#[mirror(list)]
#[derive(Debug, SimpleObject)]
pub(crate) struct ProviderConfig {
    /// The name a blueprint spells in `provider/model`, which is also the key
    /// a config write names.
    #[graphql(owned)]
    pub(crate) id: ID,
    /// The name to show.
    pub(crate) name: String,
    /// How it proves who this machine is.
    pub(crate) auth: ProviderAuthKind,
    /// Whether a run may route to it. For a provider that takes a key this is
    /// the same fact as `hasKey`, because a stored key is what puts the
    /// provider in this install; for the rest it is a switch of its own.
    pub(crate) is_enabled: bool,
    /// Whether a key is stored. The key itself never crosses the wire.
    pub(crate) has_key: bool,
    /// Where it is, for a provider that lives at an address this machine
    /// chooses.
    pub(crate) base_url: Option<String>,
    /// The region it is called in, for a provider that has regions.
    pub(crate) region: Option<String>,
    /// Its own settings, for a provider that has any.
    pub(crate) options: Option<ProviderOptions>,
}

/// How a bare model name is resolved, and what overrides a stage's own choice.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct RoutingConfig {
    /// The provider a bare model name resolves on when the order below says
    /// nothing.
    pub(crate) default_provider: String,
    /// The providers allowed to serve a bare model name, best first. Empty
    /// means `defaultProvider` alone decides.
    pub(crate) provider_order: Vec<String>,
    /// The model every stage that permits it starts on, ahead of its own list.
    pub(crate) override_model: Option<String>,
    /// The model tried after every model a stage names.
    pub(crate) fallback_model: Option<String>,
}

/// What this server itself is, rather than what it is configured to route to.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct ServerInfo {
    /// The API version this server speaks.
    pub(crate) api_version: String,
    /// What this server can do. Check these rather than calling a route and
    /// reading a 404, which also means "no such run".
    pub(crate) capabilities: Vec<String>,
    /// Whether this server was started with `--allow-admin`, so the mutations
    /// that change the machine will run rather than answer `FORBIDDEN`. Worth
    /// asking before offering a settings screen that cannot save.
    pub(crate) is_admin_enabled: bool,
    /// The numbers a client should never hardcode.
    pub(crate) limits: ServeLimits,
}

/// The daemon's configuration, with every secret left out.
///
/// Everything `updateConfig` writes is here under the same name, so a form
/// renders what it saves. A key is the exception: it reads back as `hasKey` on
/// its provider.
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct Config {
    /// How a bare model name is resolved.
    pub(crate) routing: RoutingConfig,
    /// Every provider this build knows, in a fixed order: the ones that take a
    /// key, then the ones that do not.
    pub(crate) providers: Vec<ProviderConfig>,
    /// The custom model gateways `[model_providers]` declares, name-sorted.
    pub(crate) gateways: Vec<Gateway>,
    /// Whether providers may be handed files rather than text. Zero data
    /// retention turns uploads off whatever this says.
    pub(crate) allows_file_uploads: bool,
    /// Where this server looks for blueprints, beyond the installed agents
    /// directory.
    pub(crate) blueprint_paths: Vec<String>,
    /// How many MCP servers the config declares. Read `mcpServers` for them.
    pub(crate) mcp_server_count: i32,
    /// What this server itself is.
    pub(crate) server: ServerInfo,
    /// Whether the config being served is the file on disk.
    pub(crate) health: ConfigHealth,
    /// The yolo profiles file beside the config. Read `yoloProfiles` for what
    /// is in it.
    pub(crate) yolo_file: ConfigFileStatus,
}

/// One environment or configuration check.
#[mirror(list)]
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
#[mirror]
#[derive(Debug, SimpleObject)]
pub(crate) struct DoctorReport {
    /// Whether every check passed.
    pub(crate) ok: bool,
    /// Whether the checks reached the network.
    ///
    /// False for the `doctor` field, which is offline by construction: a query
    /// dials nothing. True for `checkMachine`, which is the mutation that does.
    /// The two carry different checks, so a client showing both wants to say
    /// which one it is looking at.
    pub(crate) is_live: bool,
    /// The checks themselves, in the order they ran.
    pub(crate) checks: Vec<DoctorCheck>,
}
