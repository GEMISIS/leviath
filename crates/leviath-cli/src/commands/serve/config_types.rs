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

/// The API contract version. Held equal to the OpenAPI spec's `info.version` by
/// a test, because a version that can silently disagree with the document it
/// names is worse than no version at all.
pub(super) const API_VERSION: &str = "0.3.0";

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
    "config.gateways",
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
