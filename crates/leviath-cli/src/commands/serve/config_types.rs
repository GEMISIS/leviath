//! Request and response shapes for the config endpoints.
//!
//! Split out of `types.rs` when custom gateways pushed that file over the
//! production-line limit. The division is by concern rather than by size: the
//! config surface is the one part of the API that describes the machine's own
//! setup rather than a run, and it is the part that grows every time a new
//! kind of provider becomes configurable.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
pub(super) struct RedactedConfig {
    pub(super) default_provider: String,
    pub(super) has_anthropic_key: bool,
    pub(super) has_openai_key: bool,
    pub(super) has_google_key: bool,
    pub(super) has_openrouter_key: bool,
    pub(super) ollama_base_url: Option<String>,
    /// Every custom gateway `[model_providers]` declares, name-sorted.
    ///
    /// The four fields above can only describe the providers that existed when
    /// this struct was written, so a console could show the built-in four and
    /// was blind to any gateway a user added - which is backwards, since the
    /// people most likely to want a form for provider setup are the ones not
    /// using a first-party provider.
    pub(super) gateways: Vec<GatewayInfo>,
    pub(super) agent_paths: Vec<PathBuf>,
    pub(super) mcp_server_count: usize,
    /// The API contract this server implements, matching `info.version` in
    /// `docs/schema/openapi.json`. A test holds the two together.
    pub(super) api_version: String,
    /// What this server can do, so a client can light up features in one call.
    ///
    /// Before this, the console feature-detected by calling a route and reading
    /// a 404 as "unsupported" - fragile, because a 404 also means "no such run",
    /// and one round trip per feature.
    pub(super) capabilities: Vec<String>,
    pub(super) limits: ApiLimits,
}

/// The API contract version: this build's own version, and nothing to keep in
/// step by hand.
///
/// It was a literal, held equal to the OpenAPI spec's `info.version` by a test
/// and to the crates by nobody. The two agreed only because somebody had last
/// set both to the same string, and `cargo xtask version` writes neither - so
/// the first release after any bump would have served a version that named a
/// build it was not, silently, with the suite green.
///
/// Derived, the test that guarded the spec now guards the release: bump the
/// crates without regenerating `docs/schema/openapi.json` and it fails, which
/// is the reminder rather than the trap. `leviath-cli` takes
/// `version.workspace = true`, so this is the workspace version.
///
/// The cost is that it moves on every release, including ones no client can
/// observe. That is the right trade while `capabilities` is what a client
/// actually feature-detects on: a version that is always honest about the
/// build beats one that is occasionally wrong about the contract.
pub(super) const API_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Every capability a client may check for.
pub(super) const API_CAPABILITIES: &[&str] = &[
    "runs.envelope",
    "runs.cursor",
    "runs.search",
    "runs.search.context",
    "runs.search.logs",
    "runs.search.journal",
    "runs.fields",
    "runs.ids",
    "runs.since",
    "runs.files.listing",
    "runs.files.workdir",
    "runs.stages",
    "logs.stage",
    "logs.stream",
    "context.history.page",
    "runs.waiting_on",
    // `DELETE /api/runs/{id}` and the bulk `DELETE /api/runs`. Announced
    // because a console that cannot tell whether the route exists has to find
    // out by sending a real delete, and that probe is destructive when it
    // works. `max_ids` bounds the bulk form.
    "runs.delete",
    "runs.delete.bulk",
    // The same vocabulary on the websocket, not just on the run. Worth
    // announcing separately: a client that has it can render a parked run's
    // reason straight from the event stream, and one that doesn't has to
    // re-fetch the run every time a status arrives.
    "events.waiting_on",
    // Stage transitions and tool call start/finish as first-class frames
    // (`stage_transition`, `tool_call_started`, `tool_call_finished`) rather
    // than wrapped in the untyped `world` envelope they used to ride. Breaking
    // for a client that matched on `world`, which is why it is announced at
    // all rather than left for a client to discover.
    "events.stage_and_tool",
    // `parent_id` on `agent_spawned` names the run that spawned a sub-agent, so
    // a console can place a fan-out worker in the tree the moment it starts
    // instead of fetching every new run to find out where it hangs.
    "events.spawn_parent",
    // The `run_renamed` frame, plus `title` on `agent_status`. A run is named a
    // moment after it starts and never again, so a client without this either
    // polls every new run for its title or shows the prompt's first line until
    // something unrelated makes it re-read. Announced so a console can drop
    // that poll where the daemon has it and keep it where it does not.
    "events.title",
    "blueprints.envelope",
    "blueprints.query",
    "blueprints.manifest",
    "blueprints.validate.name",
    // `fan_outs` on the blueprint detail route: each fan-out stage's limits as
    // the daemon resolves them (`null` for unlimited, the default filled in),
    // and the manifest's `0` spelling for "no cap" on `max_workers` and
    // `max_items`. A console that has this can show and edit the caps without
    // re-implementing the parser's defaults.
    "blueprints.fan_outs",
    "tools.list",
    "scripts.read",
    // Announced whether or not `--allow-admin` was passed, which is a narrower
    // claim than the others on this list: it says this build serves the write
    // routes, not that this daemon has them mounted. `--allow-admin` decides
    // that at router construction and is deliberately not carried in
    // `ServeLimits` for a handler to read (see the note there), so a client
    // finds out the same way it finds out about the MCP admin routes - by
    // calling one and reading the status.
    "scripts.write",
    // `provider` as a fifth `kind` on the scripts routes: the drop-in model
    // providers in `~/.leviath/providers`, which are global to the machine and
    // take no `?agent=`. Separate from `scripts.read` because a build can serve
    // the four agent-owned kinds without serving this one, and a console that
    // offered the kind anyway would put an editor in front of a 400.
    "scripts.providers",
    "config.gateways",
    // `POST /api/fs/dirs`. The browser cannot open a native OS dialog onto the
    // serving machine, so the console's folder picker has to offer its own
    // "New Folder" - and one console serves every daemon version, so it needs
    // to know whether to offer the button at all rather than offering one that
    // 404s. The `GET` half is deliberately not announced: it shipped
    // unannounced, so its absence from this list proves nothing.
    "fs.mkdir",
];

/// The server's numeric limits.
///
/// This is what makes capability discovery useful rather than decorative: a
/// client that knows the feature exists still has to guess the page cap, the
/// file-size cap and the tracked-file cap, and every one of those guesses would
/// be hardcoded and eventually wrong.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ApiLimits {
    /// Largest `limit` on `GET /api/runs`; larger values are clamped.
    pub(super) max_limit: usize,
    /// Most ids one `ids=` batch may name.
    pub(super) max_ids: usize,
    /// Largest file body `?path=` returns.
    pub(super) max_file_bytes: u64,
    /// Most entries one directory listing returns.
    pub(super) max_listing_entries: usize,
    /// How many runs a filesystem-reading search examines before reporting
    /// `scan_truncated`.
    pub(super) max_search_scan: usize,
    /// How much of each stage log a search reads, from the end.
    pub(super) search_log_tail_bytes: u64,
    /// Largest `limit` on the context-history route.
    pub(super) max_history_limit: usize,
    /// How many distinct modified paths a run records before
    /// `modified_files_truncated` is set.
    pub(super) max_tracked_modified_files: usize,
}

impl ApiLimits {
    /// Read from the constants the handlers actually use, so the two cannot
    /// drift into disagreeing.
    pub(super) fn current() -> Self {
        Self {
            max_limit: super::runs::MAX_LIMIT,
            max_ids: super::runs::MAX_IDS,
            max_file_bytes: super::agents::MAX_FILE_READ_BYTES,
            max_listing_entries: super::agents::MAX_LISTING_ENTRIES,
            max_search_scan: super::runs::MAX_SEARCH_SCAN,
            search_log_tail_bytes: super::runs::SEARCH_LOG_TAIL_BYTES,
            max_history_limit: super::agents::HISTORY_MAX_LIMIT,
            max_tracked_modified_files: leviath_core::run_meta::MAX_TRACKED_MODIFIED_FILES,
        }
    }
}

/// Body of `PUT /api/config` (admin-only). Every field is optional; a present
/// field is written, an absent one is left untouched. Mirrors what `lev setup`
/// writes, so a newcomer can configure providers entirely from the browser.
#[derive(Debug, Default, Deserialize)]
pub(super) struct WriteConfigReq {
    pub(super) default_provider: Option<String>,
    pub(super) default_model: Option<String>,
    pub(super) anthropic_key: Option<String>,
    pub(super) openai_key: Option<String>,
    pub(super) google_key: Option<String>,
    pub(super) openrouter_key: Option<String>,
    pub(super) ollama_base_url: Option<String>,
    /// Gateways to add or update, by name. Absent means "change none", the
    /// same partial-update rule every field above follows: a gateway this list
    /// does not mention is left exactly as it was.
    #[serde(default)]
    pub(super) gateways: Option<Vec<GatewayWrite>>,
    /// Gateways to remove, by name. Separate from the list above because
    /// omitting a gateway there means "leave it alone", so there would
    /// otherwise be no way to say "delete it" without sending the whole set
    /// and reintroducing the read-modify-write hazard this endpoint avoids.
    #[serde(default)]
    pub(super) remove_gateways: Option<Vec<String>>,
}

/// One custom gateway as `GET /api/config` reports it.
///
/// The key is never served, only whether one is set, exactly like the
/// `has_*_key` booleans beside it.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub(super) struct GatewayInfo {
    /// The name an agent references, and the `[model_providers]` table key.
    pub(super) name: String,
    /// Where the gateway lives, when the entry sets one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) base_url: Option<String>,
    /// Whether a key is configured for it.
    pub(super) has_api_key: bool,
    /// The Rhai provider script backing it, when the entry names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) script: Option<String>,
    /// The names of any extra keys the entry carries, without their values.
    ///
    /// `extra` is forwarded verbatim into the script's `initialize`, so people
    /// put credentials in it: a second token, a signing secret. Serving those
    /// values would leak precisely what `has_api_key` exists to avoid, so only
    /// the names are reported - enough for a form to show that the fields are
    /// there and not enough to disclose one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) extra_keys: Vec<String>,
}

/// One custom gateway as `PUT /api/config` accepts it.
#[derive(Debug, Deserialize)]
pub(super) struct GatewayWrite {
    /// Which gateway this is. Created when no entry has this name.
    pub(super) name: String,
    /// Absent leaves whatever the entry already had.
    #[serde(default)]
    pub(super) base_url: Option<String>,
    /// Absent leaves the existing key in place, which is what lets a console
    /// edit a gateway's URL without having to know its key or send it back.
    #[serde(default)]
    pub(super) api_key: Option<String>,
    /// Absent leaves the existing script name.
    #[serde(default)]
    pub(super) script: Option<String>,
}

/// Body of `POST /api/config/validate` — a format-only key check (no network,
/// no persistence), mirroring the `lev setup` wizard's inline validation.
#[derive(Debug, Deserialize)]
pub(super) struct ValidateKeyReq {
    pub(super) provider: String,
    pub(super) key: String,
    /// A gateway's base URL, checked alongside the key when present.
    ///
    /// A custom gateway's key has no house format to check - that is what
    /// makes it custom - so for one the useful pre-flight is the URL, which is
    /// the field people actually get wrong.
    #[serde(default)]
    pub(super) base_url: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ValidateKeyResp {
    pub(super) valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) message: Option<String>,
}
