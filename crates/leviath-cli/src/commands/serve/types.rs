//! Shared types: ServerEvent, AppState, request/response structs, error types.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Args;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use super::events::ServerEvent;
use crate::config::Config;
use crate::daemon::config_reload::ConfigReloader;

// ─── CLI ─────────────────────────────────────────────────────────────────────

/// Arguments for `lev serve`.
#[derive(Args, Clone)]
pub struct ServeArgs {
    /// Port to listen on
    #[arg(short, long, default_value = "3000")]
    pub port: u16,

    /// Host to bind to
    #[arg(short = 'H', long, default_value = "127.0.0.1")]
    pub host: String,

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

// ─── Shared state ────────────────────────────────────────────────────────────

/// What every request handler is given: the config, the event fan-out, and the
/// control-socket client that reaches the daemon.
#[derive(Clone)]
pub(crate) struct AppState {
    /// Where the config comes from, rather than a copy of it.
    ///
    /// A snapshot taken at start-up was wrong in both directions: an edit made
    /// through `PUT /api/config` was written to disk and never read back, so
    /// reloading the page showed the old value and the edit read as lost; and
    /// an edit made anywhere else - `lev setup`, an editor, the daemon's own
    /// config - was invisible for the life of the process. `lev serve` is a
    /// separate process from `lev daemon`, so restarting the daemon did not
    /// help either (issue #532).
    ///
    /// This is the same [`ConfigReloader`] the daemon uses for its spawn-time
    /// config: mtime-checked, last-good on a parse failure. Read it through
    /// [`AppState::current_config`].
    pub(super) config: Arc<ConfigReloader>,
    pub(super) event_tx: broadcast::Sender<ServerEvent>,
    /// Client for the shared-world daemon's control socket. Agent actions
    /// (spawn/cancel/message/interactions) go through this; read endpoints still
    /// observe the runs dir the daemon persists to.
    pub(super) control: leviath_runtime::control_socket::ControlClient,
    /// Paths + seams for the MCP management endpoints.
    pub(super) mcp: super::mcp::McpAdmin,
    /// The spawn-request restrictions from [`ServeArgs`], resolved once at
    /// startup so every handler reads the same decision.
    pub(super) limits: Arc<ServeLimits>,
    /// The last answer to "is there anything newer", for `GET /api/update`.
    ///
    /// On the state rather than in a `static` so two servers in one process -
    /// or two tests - cannot write into each other's answer.
    pub(super) update_check: super::update_cache::UpdateCheckCache,
    /// The update runs `POST /api/update` has started, and the machine to start
    /// another on. On the state for the same reason the cache above is.
    pub(super) update_jobs: super::update_job::UpdateJobs,
}

impl AppState {
    /// The config as it is on disk right now.
    ///
    /// Every handler reads it through here rather than holding one, so an edit
    /// - through this API or from outside - is visible to the next request.
    pub(super) fn current_config(&self) -> Arc<Config> {
        self.config.current()
    }
}

/// What this server refuses regardless of who is asking.
///
/// A valid API token proves the caller is allowed to *use* the server; it does
/// not mean they should be able to reconfigure the machine or point an agent at
/// the filesystem root. These are the operator's answers to that, fixed at
/// startup rather than negotiable per request.
///
/// `--allow-admin` is deliberately absent: it decides whether a route is
/// *mounted at all*, so it is consumed once at router construction rather than
/// carried here for a handler to consult. An unmounted route 404s; a mounted one
/// guarded by a field is one refactor away from being reachable.
#[derive(Debug, Clone, Default)]
pub(super) struct ServeLimits {
    /// `--workdir-root`: the directory agent workdirs must sit under.
    pub(super) workdir_root: Option<PathBuf>,
    /// `--no-remote-yolo`: whether a spawn request may waive approvals, with
    /// either `"yolo": true` or an `"allow"` list.
    pub(super) no_remote_yolo: bool,
    /// `[security] allow_local_network`: whether a completion webhook may point
    /// at loopback, private or link-local addresses.
    pub(super) allow_local_network: bool,
    /// `--no-remote-seed-commands`: whether every spawn is treated as having
    /// asked for `"no_seed_commands": true`.
    pub(super) no_remote_seed_commands: bool,
    /// The request cap and timeout in force, after the flags and `[serve]`
    /// have been reconciled. Reported by `GET /api/config`.
    pub(super) request_limits: super::request_limits::RequestLimits,
}

impl ServeLimits {
    /// Check a requested agent workdir against `--workdir-root`.
    ///
    /// Uses the same symlink-aware containment the file tools use, so a symlink
    /// under the root cannot be used to point an agent outside it.
    /// Check a requested completion webhook against the same SSRF policy every
    /// model-supplied URL goes through.
    ///
    /// The URL arrives in a `POST /api/agents` body, is persisted, and is POSTed
    /// to when the run finishes - from inside the trust boundary, and with
    /// retries. Unchecked, `"callback_url": "http://169.254.169.254/…"` made the
    /// daemon a repeatable request primitive against the cloud metadata service
    /// and anything else on the local network, on behalf of a caller that
    /// `--workdir-root` and `--no-remote-yolo` exist to keep at arm's length.
    ///
    /// `allow_local_network` mirrors the config setting: an operator who
    /// deliberately points webhooks at a service on the same host can, and
    /// everyone else cannot.
    pub(super) fn check_callback_url(&self, url: &str) -> Result<(), String> {
        let parsed = url
            .parse::<url::Url>()
            .map_err(|e| format!("callback_url is not a URL: {e}"))?;
        leviath_net::check_url(&parsed, self.allow_local_network)
            .map_err(|e| format!("callback_url is not allowed: {e}"))
    }

    /// Check the approval waivers a spawn request asked for against
    /// `--no-remote-yolo`.
    ///
    /// `yolo` and `allow` are the same lever. `{"allow": ["*"]}` is read by
    /// `resolve_policy` through the same wildcard entry `--yolo` writes, so
    /// guarding only the `yolo` field left the operator's refusal bypassable by
    /// spelling it the other way. Any `allow` is refused rather than just the
    /// wildcard: `{"allow": ["shell"]}` is not meaningfully weaker on a server
    /// somebody deliberately hardened, and "allow is yolo" is a rule an
    /// operator can hold in their head. A per-agent grant belongs in the
    /// operator's own config, under `[agent_tool_permissions.<agent>]`.
    pub(super) fn check_launch_overrides(
        &self,
        yolo: bool,
        allow: &[String],
    ) -> Result<(), String> {
        if !self.no_remote_yolo {
            return Ok(());
        }
        match (yolo, allow.is_empty()) {
            (false, true) => Ok(()),
            _ => Err(
                "this server refuses `yolo` and `allow` on spawn requests (--no-remote-yolo)"
                    .to_string(),
            ),
        }
    }

    pub(super) fn check_workdir(&self, workdir: &std::path::Path) -> Result<(), String> {
        let Some(root) = &self.workdir_root else {
            return Ok(());
        };
        match leviath_core::resolves_within(workdir, root) {
            true => Ok(()),
            false => Err(format!(
                "workdir '{}' is outside the configured --workdir-root '{}'",
                workdir.display(),
                root.display()
            )),
        }
    }
}

// ─── Error response ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(super) struct ErrorResponse {
    pub(super) error: String,
}

/// The `(status, body)` pair every fallible handler returns.
pub(super) type ApiError = (axum::http::StatusCode, axum::response::Json<ErrorResponse>);

/// Build a `(status, JSON error)` response tuple.
pub(super) fn err(code: axum::http::StatusCode, message: String) -> ApiError {
    (code, axum::response::Json(ErrorResponse { error: message }))
}

/// A daemon reply this handler has no arm for.
///
/// 500 rather than 502: the reply decoded, so the two still speak the same
/// protocol; the handler simply did not expect this answer to this request.
pub(super) fn unexpected_response(
    other: leviath_runtime::control_socket::ControlResponse,
) -> ApiError {
    err(
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        format!("Unexpected daemon response: {other:?}"),
    )
}

/// The status for a control request the daemon answers with `Ok { ok }`.
///
/// `success` when it did the thing; 404 carrying `not_found` when it could
/// not, since every such request names a run or an interaction and "could
/// not" means the daemon had nothing by that name in the right state. Five
/// handlers used to carry this four-arm match each.
pub(super) fn daemon_ok(
    reply: std::io::Result<leviath_runtime::control_socket::ControlResponse>,
    success: axum::http::StatusCode,
    not_found: String,
) -> Result<axum::http::StatusCode, ApiError> {
    match reply {
        Ok(leviath_runtime::control_socket::ControlResponse::Ok { ok: true }) => Ok(success),
        Ok(leviath_runtime::control_socket::ControlResponse::Ok { ok: false }) => {
            Err(err(axum::http::StatusCode::NOT_FOUND, not_found))
        }
        Ok(other) => Err(unexpected_response(other)),
        Err(e) => Err(daemon_error(e)),
    }
}

/// The response for a control request the daemon did not answer.
///
/// Two different failures, told apart by the error's kind, because they have
/// different remedies:
///
/// - **503 Service Unavailable**: the daemon is not reachable right now. It
///   may be restarting (the client already waited a grace period for that),
///   stopped, or wedged. Retrying later, or `lev daemon restart`, is the fix.
/// - **502 Bad Gateway**: the daemon answered, but this server could not
///   understand it - the daemon was updated under a running `lev serve`, and
///   the two no longer speak the same protocol. Retrying cannot help; the
///   message says what does, which is restarting `lev serve`.
pub(super) fn daemon_error(e: std::io::Error) -> ApiError {
    match e.kind() {
        std::io::ErrorKind::Unsupported => err(
            axum::http::StatusCode::BAD_GATEWAY,
            format!("This server needs a restart: {e}"),
        ),
        _ => err(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            format!("Daemon not reachable: {e}"),
        ),
    }
}

// ─── Pagination ─────────────────────────────────────────────────────────────

/// One page of a collection: the shape every paginated route returns.
///
/// One envelope rather than one per route, so a client writes the paging loop
/// once. The house rule for which routes get it: **collections that grow
/// without bound are paginated; bounded catalogs stay bare arrays.** Runs
/// accumulate forever and are never pruned, so they are paged; `/api/models`
/// and `/api/mcp/servers` are sized by what the user configured, and
/// `/api/agents/tree` is a tree, where `next_cursor` would not mean anything.
#[derive(Debug, Serialize)]
pub(super) struct Page<T> {
    /// This page's items, in the requested order.
    pub(super) items: Vec<T>,
    /// Pass back as `cursor` for the next page. `null` means this was the last
    /// one - clients loop until null rather than counting against `total`,
    /// because `total` can move underneath a walk.
    ///
    /// Only ever emitted when a further item is known to exist: the handler
    /// takes `limit + 1` and keeps `limit`.
    pub(super) next_cursor: Option<String>,
    /// How many items matched this query, at the moment of this request.
    ///
    /// `null` when the answer would be a guess - see `scan_truncated`. A count
    /// derived from a partial scan is worse than no count, because a UI renders
    /// it as fact and a user paginates against it.
    pub(super) total: Option<usize>,
    /// Unix **seconds** at which this page was built, on the server's clock.
    ///
    /// The watermark for a client's next `since=`: round-tripping the server's
    /// own timestamp keeps a polling client from missing or re-fetching work
    /// because its clock disagrees.
    pub(super) server_time: i64,
    /// Set when a filesystem-scanning filter gave up before examining every
    /// candidate, so this page is a prefix of the truth rather than all of it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(super) scan_truncated: bool,
    /// Ids from an `ids=` batch fetch that no longer exist. Absent otherwise.
    ///
    /// A missing id is reported rather than 404ing the whole request: a batch
    /// refresh of ten runs should not fail because one was deleted.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) missing: Vec<String>,
}

impl<T> Page<T> {
    /// A page with nothing unusual about it: no truncation, no missing ids.
    pub(super) fn new(
        items: Vec<T>,
        next_cursor: Option<String>,
        total: Option<usize>,
        server_time: i64,
    ) -> Self {
        Self {
            items,
            next_cursor,
            total,
            server_time,
            scan_truncated: false,
            missing: Vec::new(),
        }
    }
}

/// One run in a `GET /api/runs` page.
#[derive(Debug, Serialize)]
pub(super) struct RunItem {
    /// The run's metadata, redacted, and narrowed to the requested `fields`.
    ///
    /// A `serde_json::Value` rather than a second `RunSummary` struct: field
    /// projection is a runtime choice, and serializing through `RunMeta`'s own
    /// serde means there is no parallel shape that can drift away from it.
    pub(super) meta: serde_json::Value,
    /// Why this run matched, when the request carried a `q`. Empty otherwise.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) highlights: Vec<Highlight>,
}

/// Where a search matched, and enough text to show a user why.
///
/// The part of search that cannot be done in the browser: the console never has
/// a run's transcript, so without this a deep match is an unexplained result.
#[derive(Debug, Serialize)]
pub(super) struct Highlight {
    /// What matched: a `RunMeta` field name, `metadata.<key>`,
    /// `modified_files`, `context.<region>`, `logs.output`, `logs.operational`,
    /// or `journal.tool.<tool_name>`.
    pub(super) field: String,
    /// The matching text with a little either side, elided at any cut end.
    pub(super) snippet: String,
    /// Which stage the match came from, for the sources that have one. A client
    /// can pass it straight to `GET /api/agents/{id}/logs?stage=`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) stage: Option<usize>,
}

// ─── Blueprint types ────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(super) struct BlueprintInfo {
    pub(super) name: String,
    pub(super) version: String,
    pub(super) description: String,
    pub(super) path: String,
    pub(super) stages: Vec<String>,
    /// The manifest text, carried but never listed.
    ///
    /// Discovery has already read and parsed this file to fill in everything
    /// above, so the detail route hands back the text it read rather than
    /// opening the file a second time. That is not only cheaper: a second read
    /// could fail on a file the first read succeeded on, and the only honest
    /// answer to that would be an error arm no test can reach.
    ///
    /// Skipped when serializing, so `GET /api/blueprints` stays a catalog.
    /// [`BlueprintDetail`] is what puts it on the wire.
    #[serde(skip)]
    pub(super) manifest: String,
}

/// One context region of a blueprint, as the API reports it.
///
/// The console showed a blueprint's stages and nothing about its memory, so a
/// person editing an agent could see what it *does* and not what it *keeps* -
/// which is the half that decides whether it can do the job on a small window.
#[derive(Debug, Serialize)]
pub(super) struct RegionInfo {
    /// The region's name - the one an agent passes to `context_write`.
    pub(super) name: String,
    /// Its kind, as written in the manifest (`pinned`, `sliding_window`, …).
    pub(super) kind: String,
    /// What it is for, when the blueprint says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) description: Option<String>,
    /// Whether the description is also shown to the model, rather than only
    /// being documentation. A console offering an editor needs this to render
    /// the toggle honestly.
    pub(super) describe_in_prompt: bool,
    /// Its token ceiling, resolved as far as the manifest alone allows.
    pub(super) max_tokens: usize,
}

/// One fan-out stage of a blueprint, as the API reports it.
///
/// A console editing a fan-out stage showed `max_iterations` and nothing else,
/// so the only limit in sight read as a retry count and the two that decide
/// how many workers a stage gets, `max_workers` and `max_items`, were
/// invisible unless you read the TOML. This is the same block resolved the
/// way the daemon resolves it: defaults filled in, and `null` for a cap that
/// is not there.
#[derive(Debug, Serialize)]
pub(super) struct FanOutInfo {
    /// The stage's name.
    pub(super) stage: String,
    /// A separate installed blueprint the workers run as, when that is how the
    /// stage picks its worker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) worker_agent: Option<String>,
    /// A stage of this blueprint the workers run as, when that is how.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) worker_stage: Option<String>,
    /// A discovery query matched against installed agents, when that is how.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) worker_query: Option<String>,
    /// The stage that reconciles the workers' results, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) merge_stage: Option<String>,
    /// How many workers run at once. `null` is unlimited (`max_workers = 0` in
    /// the manifest); a stage that names no cap gets the default, and that
    /// default is what appears here.
    pub(super) max_workers: Option<usize>,
    /// How many work items the split may produce at all. `null` is unlimited
    /// (`max_items = 0`, or no key).
    pub(super) max_items: Option<usize>,
    /// `continue` or `fail_all`, as the manifest spells them.
    pub(super) on_worker_failure: String,
    /// The region the consolidated worker report is written to, when the
    /// stage names one; `null` means the conversation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) results_region: Option<String>,
}

/// One blueprint, with the manifest text behind it.
///
/// The detail route only. A listing that carried one of these per blueprint
/// would send every manifest on the machine to answer "what agents are there",
/// which is why this is a separate shape rather than an extra field on
/// [`BlueprintInfo`].
///
/// Flattened, so the detail route's JSON is what it always was plus
/// `manifest`, and a client reading only the old fields is unaffected.
#[derive(Debug, Serialize)]
pub(super) struct BlueprintDetail {
    #[serde(flatten)]
    pub(super) info: BlueprintInfo,
    /// The blueprint's context regions.
    ///
    /// On the detail route rather than the listing, for the same reason the
    /// manifest is: answering "what agents are there" should not cost every
    /// region of every agent on the machine.
    pub(super) regions: Vec<RegionInfo>,
    /// The blueprint's fan-out stages, with their limits as the daemon will
    /// apply them. Empty for a blueprint that never fans out.
    pub(super) fan_outs: Vec<FanOutInfo>,
    /// The manifest exactly as it is on disk.
    ///
    /// Without this a console has no way to read what it is editing: naming
    /// the file in `path` is not the same as being able to open it, since the
    /// browser cannot, and the fallbacks it is left with (a draft in local
    /// storage, or a copy bundled at build time) are both disconnected from
    /// the file the daemon actually runs.
    pub(super) manifest: String,
}

/// Query for `GET /api/blueprints`.
#[derive(Deserialize, Default)]
pub(super) struct BlueprintsQuery {
    pub(super) limit: Option<usize>,
    pub(super) cursor: Option<String>,
    /// Case-insensitive substring over name, description and stage names.
    pub(super) q: Option<String>,
    /// `name` (default) or `version`.
    pub(super) sort: Option<String>,
    /// `asc` (default) or `desc`. Ascending by name is the catalog order a
    /// person reads.
    pub(super) order: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct CreateBlueprintReq {
    pub(super) name: String,
    pub(super) manifest: String,
}

#[derive(Deserialize)]
pub(super) struct UpdateBlueprintReq {
    pub(super) manifest: String,
}

#[derive(Deserialize)]
pub(super) struct ValidateBlueprintReq {
    pub(super) manifest: String,
    /// The blueprint this manifest is an edit of, when it is one.
    ///
    /// The lint resolves an agent's own `tools/*.rhai` relative to a
    /// directory, so validating an existing agent without saying which one
    /// reports every tool it defines as unknown. A manifest genuinely typed
    /// from nothing has no directory to name, and omitting this keeps the
    /// old behaviour for that case.
    #[serde(default)]
    pub(super) name: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct ValidateResponse {
    pub(super) valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) errors: Option<Vec<String>>,
    /// Lint findings that do not make the blueprint invalid: fields left to a
    /// default, a stage that can block on a human, a broad `[read_paths]`
    /// entry. Absent when there are none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) warnings: Option<Vec<String>>,
}

impl ValidateResponse {
    /// The response for a manifest that did not parse or did not validate.
    pub(super) fn invalid(errors: Vec<String>) -> Self {
        Self {
            valid: false,
            errors: Some(errors),
            warnings: None,
        }
    }
}

// ─── Agent types ────────────────────────────────────────────────────────────

#[derive(Default, Deserialize)]
pub(super) struct SpawnAgentReq {
    pub(super) blueprint: String,
    pub(super) task: String,
    pub(super) model: Option<String>,
    /// Override the blueprint's max sub-agent tree depth.
    pub(super) max_depth: Option<usize>,
    /// Approve every tool call for this run.
    #[serde(default)]
    pub(super) yolo: bool,
    /// Tools to allow outright for this run.
    #[serde(default)]
    pub(super) allow: Vec<String>,
    /// Refuse this run's `seed = { command = ... }` regions, which would
    /// otherwise execute at spawn before any approval prompt.
    #[serde(default)]
    pub(super) no_seed_commands: bool,
    pub(super) workdir: Option<String>,
    /// Literal seed content for named caller-input regions, keyed by region name.
    #[serde(default)]
    pub(super) regions: HashMap<String, String>,
    #[serde(default)]
    pub(super) metadata: HashMap<String, String>,
    pub(super) callback_url: Option<String>,
    /// Optional shared secret; when set, completion webhooks carry an
    /// `X-Leviath-Signature: sha256=<hex>` HMAC of the body keyed on this secret.
    pub(super) callback_secret: Option<String>,
    /// Ask for the run's final output in a particular shape, overriding what the
    /// blueprint declares. Any label works - `markdown`, `xml`, `a2ui`, a media
    /// type, your own - because nothing converts between shapes: the label and
    /// instructions are handed to the model, which produces the bytes.
    pub(super) output_format: Option<String>,
    /// Extra guidance about that shape. This is how an unusual format gets
    /// explained to the model.
    pub(super) output_instructions: Option<String>,
    /// A JSON Schema the final output must satisfy. The only thing that ever
    /// inspects the answer's contents, and only because you asked: a submission
    /// that fails is refused back to the agent to correct.
    ///
    /// Naming `output_format` without this drops a schema the blueprint
    /// declared, since a check written for one shape says nothing about another.
    pub(super) output_schema: Option<serde_json::Value>,
}

/// A run's final output as the API serves it.
///
/// `content` is exactly what the agent submitted - nothing re-serializes or
/// reformats it - and `format` is the label it was asked for. A UI that renders
/// a2ui differently from markdown matches on that string; the server never does.
#[derive(Serialize, Debug, Clone)]
pub(crate) struct FinalOutputResp {
    pub content: String,
    pub format: Option<String>,
    /// The stage that produced it.
    pub stage: String,
    /// Unix seconds at submission.
    pub submitted_at: i64,
    /// Whether the answer hit the size cap and was cut short.
    pub truncated: bool,
    /// Files the run produced, as workdir-relative paths. Fetch one with
    /// `GET /api/agents/{id}/files?path=`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
}

impl From<leviath_core::output::FinalOutput> for FinalOutputResp {
    fn from(o: leviath_core::output::FinalOutput) -> Self {
        Self {
            content: o.content,
            format: o.format,
            stage: o.stage,
            submitted_at: o.submitted_at,
            truncated: o.truncated,
            artifacts: o.artifacts,
        }
    }
}

#[derive(Serialize, Debug)]
pub(super) struct SpawnAgentResp {
    pub(super) agent_id: String,
    pub(super) run_id: String,
}

#[derive(Deserialize)]
pub(super) struct ListAgentsQuery {
    pub(super) status: Option<String>,
}

/// Does `filter` name this run's status?
///
/// `RunStatus` reaches a client two different ways: `Json<RunMeta>` serializes
/// it through serde, which is `snake_case` (`waiting_input`), while the status
/// filter compared it through `Display`, which is PascalCase, lowercased
/// (`waitinginput`). So a client that took a status out of one response and fed
/// it back as a filter got nothing, on exactly the two multi-word variants where
/// it is least obvious why.
///
/// Normalizing both sides - lowercase, and drop `_` and `-` - accepts every
/// spelling of the same status, including the two that already worked. That
/// makes this strictly wider than the old comparison, so nothing a client does
/// today can start failing.
pub(super) fn status_matches(status: &crate::runstate::RunStatus, filter: &str) -> bool {
    fn normalize(s: &str) -> String {
        s.chars()
            .filter(|c| *c != '_' && *c != '-')
            .flat_map(char::to_lowercase)
            .collect()
    }
    normalize(&format!("{status}")) == normalize(filter)
}

#[derive(Serialize)]
pub(super) struct AgentResultResp {
    pub(super) run_id: String,
    pub(super) status: String,
    /// The tail of the last stage's log. Kept as-is: it predates
    /// `final_output`, callers depend on it, and it answers a different
    /// question - what the run *did*, rather than what it concluded.
    pub(super) output: String,
    /// What the agent handed back, when it submitted anything. This is the
    /// run's answer; prefer it over `output` when present.
    pub(super) final_output: Option<FinalOutputResp>,
    pub(super) error: Option<String>,
    pub(super) prompt_tokens: usize,
    pub(super) completion_tokens: usize,
}

/// Query for `GET /api/agents/{id}/logs`.
#[derive(Deserialize, Default)]
pub(super) struct LogsQuery {
    /// How many **bytes** from the end to return (default 32 KiB). Bytes, not
    /// lines - the OpenAPI description said lines and was wrong.
    pub(super) tail: Option<u64>,
    /// Which stage: a numeric index, or `all` for every stage joined oldest
    /// first. Absent means the stage the run is on now.
    pub(super) stage: Option<String>,
    /// `output` (default) for the assistant's readable output, or `logs` for
    /// the operational stream (`[tool]`, `[Tokens: …]`, `[error]`).
    ///
    /// Kept as two separate streams rather than interleaved: they have
    /// different audiences and no shared clock in the files, so merging them
    /// would be presenting a guess at ordering as a fact.
    pub(super) stream: Option<String>,
}

impl LogsQuery {
    /// Resolve `stage` into a [`StageSelector`], or report the bad value.
    pub(super) fn selector(&self) -> Result<crate::runstate::StageSelector, String> {
        use crate::runstate::StageSelector;
        match self.stage.as_deref() {
            None => Ok(StageSelector::Current),
            Some("all") => Ok(StageSelector::All),
            Some(other) => other
                .parse::<usize>()
                .map(StageSelector::Index)
                .map_err(|_| format!("Invalid stage '{other}': expected a stage index or 'all'")),
        }
    }

    /// Resolve `stream`, or report the bad value.
    pub(super) fn log_stream(&self) -> Result<crate::runstate::LogStream, String> {
        use crate::runstate::LogStream;
        match self.stream.as_deref() {
            None | Some("output") => Ok(LogStream::Output),
            Some("logs") => Ok(LogStream::Operational),
            Some(other) => Err(format!(
                "Invalid stream '{other}': expected 'output' or 'logs'"
            )),
        }
    }
}

/// Query for `GET /api/agents/{id}/context/history`.
#[derive(Deserialize, Default)]
pub(super) struct HistoryQuery {
    /// How many points to return. Capped lower than the run listing's, because
    /// each item carries a whole context window.
    pub(super) limit: Option<usize>,
    /// Continuation token from the previous page's `next_cursor`.
    pub(super) cursor: Option<String>,
    /// `asc` (default, chronological - and what the unpaged response gave) or
    /// `desc` to start from the most recent point.
    pub(super) order: Option<String>,
}

/// Query for `GET /api/agents/{id}/files`.
///
/// With `path`, reads that file. Without, lists - the same idiom `GET
/// /api/fs/dirs` uses for the folder picker.
#[derive(Deserialize, Default)]
pub(super) struct FileQuery {
    /// The file to read. Relative paths resolve against the run's workdir;
    /// absolute paths are accepted but must still land inside it. Absent means
    /// "list", and a directory lists rather than erroring.
    pub(super) path: Option<String>,
    /// `modified` (default) or `workdir`. See [`FileSource`].
    pub(super) source: Option<String>,
    /// Include dot-prefixed entries when listing a directory. Off by default,
    /// mirroring `DirsQuery`.
    #[serde(default)]
    pub(super) hidden: bool,
    /// Byte offset to start reading at. Absent means the beginning.
    ///
    /// A run's artifact can be far larger than one response, so a caller pages
    /// through it: read a window, then ask again from the `next_offset` the
    /// response carries.
    pub(super) offset: Option<u64>,
}

/// Whether a count is zero, for `skip_serializing_if`.
fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Which question a listing answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileSource {
    /// What the run recorded modifying. Free, but a claim about the run rather
    /// than about the disk, and capped at record time.
    Modified,
    /// What is in the run's working directory now, one level at a time.
    Workdir,
}

impl FileQuery {
    /// Resolve `source`, or report the bad value.
    pub(super) fn file_source(&self) -> Result<FileSource, String> {
        match self.source.as_deref() {
            None | Some("modified") => Ok(FileSource::Modified),
            Some("workdir") => Ok(FileSource::Workdir),
            Some(other) => Err(format!(
                "Invalid source '{other}': expected 'modified' or 'workdir'"
            )),
        }
    }
}

/// Response of `GET /api/agents/{id}/files`: either one file's contents, or a
/// listing.
///
/// Untagged, with the listing carrying a literal `kind` field, so a client
/// discriminates on a value rather than by trying parses until one fits.
/// `FileContentResp` is unchanged and still serializes exactly as before, so
/// existing readers of `?path=<file>` see no difference at all.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(super) enum FileOrListing {
    File(FileContentResp),
    /// Boxed because it is much the larger variant, and every file read would
    /// otherwise pay for its size.
    Listing(Box<RunFileListing>),
}

/// Response of `GET /api/agents/{id}/stages`: the run's per-stage ledger.
///
/// The one thing the runtime records per run that no route served. Everything
/// here is already on disk in `stages.json` and already read by `lev stages`;
/// a client over HTTP had to reconstruct the interesting part by diffing
/// `context/history` snapshots, which is expensive and cannot see a stage that
/// ran and wrote nothing (#388).
///
/// Each record carries a price as well as a token count, and a split of both by
/// each stay in the stage. Pricing stays on this side deliberately: `cost_usd`
/// is `None` for unknown rather than zero and `cost_is_exact` says whether the
/// figure is the invoice or a reconstruction of it, and a console multiplying
/// tokens by a rate card of its own would produce a fourth answer that
/// disagrees with all three (#630).
///
/// Not paginated. The list is bounded by the blueprint's stage count - a dozen
/// at the top end - so a cursor would be ceremony over a short array. The
/// records themselves are open-ended in width because `region_tokens` has one
/// entry per region and `visits` one entry per stay, which is the reason this is
/// its own route rather than a field on the run listing.
#[derive(Debug, Serialize)]
pub(super) struct RunStagesResp {
    /// The run these stages belong to, echoed so a response is self-describing
    /// when it has been passed around.
    pub(super) run_id: String,
    /// Per-stage records in blueprint order, exactly as recorded.
    pub(super) stages: Vec<leviath_core::run_meta::StageRecord>,
}

/// Response of `GET /api/agents/{id}/files` with no file named.
#[derive(Debug, Serialize)]
pub(super) struct RunFileListing {
    /// Always `"listing"`. What a client checks to tell the two shapes apart.
    pub(super) kind: &'static str,
    /// Which question this answers: `modified` or `workdir`.
    pub(super) source: &'static str,
    /// The directory listed, or the workdir for a `modified` listing.
    pub(super) path: String,
    /// Where "up one level" goes, or `null` at the workdir root.
    pub(super) parent: Option<String>,
    /// The run's working directory, which paths are relative to.
    pub(super) workdir: String,
    pub(super) entries: Vec<RunFileEntry>,
    /// Whether `entries` stops short of the directory's real contents.
    pub(super) truncated: bool,
    /// Whether the run hit the tracked-modified-files cap, so its recorded list
    /// is a prefix and the remaining names were never stored anywhere.
    ///
    /// Exposed because the alternative - a client subtracting
    /// `modifying_tool_calls` from `entries.len()` - is wrong, and was the
    /// original "+N more" bug. Use `source=workdir` for ground truth about what
    /// is actually on disk.
    pub(super) modified_files_truncated: bool,
    /// Successful **modifying tool calls**, which is not a file count: a run
    /// that edits one file three times records three. Named for what it counts.
    pub(super) modifying_tool_calls: usize,
}

/// One entry in a [`RunFileListing`].
#[derive(Debug, Serialize)]
pub(super) struct RunFileEntry {
    pub(super) name: String,
    /// Relative to the run's workdir where possible, so a client can pass it
    /// straight back as `?path=`.
    pub(super) path: String,
    pub(super) is_dir: bool,
    /// `null` when the entry could not be stat-ed.
    pub(super) size: Option<u64>,
    /// False for a recorded path that has since been deleted.
    pub(super) exists: bool,
    /// True for a recorded path that resolves outside the workdir - possible
    /// when a tool was handed an absolute path. Reported rather than hidden.
    pub(super) outside_workdir: bool,
}

/// Response of `GET /api/agents/{id}/files`: one file the run wrote, as text.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct FileContentResp {
    /// The resolved absolute path that was read.
    pub(super) path: String,
    /// The file's full size in bytes - larger than `content` when `truncated`.
    pub(super) size: u64,
    /// Where this window starts, in bytes. Not always the `offset` that was
    /// asked for: an offset landing mid-character is moved forward to the next
    /// character boundary, so the pages of a file line up.
    ///
    /// Omitted when it is zero, which keeps a whole-file read serializing
    /// exactly as it did before paging existed. Adding a key to that response
    /// would be harmless for most clients and is still not worth doing to all
    /// of them for a field only a paging caller reads.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(super) offset: u64,
    /// Where to start the next request to continue reading. `null` when this
    /// window reached the end of the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) next_offset: Option<u64>,
    /// This window's bytes as UTF-8, capped at
    /// [`MAX_FILE_READ_BYTES`](super::agents::MAX_FILE_READ_BYTES).
    pub(super) content: String,
    /// Whether the file continues past this window. Read on from `next_offset`.
    pub(super) truncated: bool,
}

// ─── Doctor types ───────────────────────────────────────────────────────────

/// Response of `GET /api/doctor`: the `lev doctor` checks, as data.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct DoctorResp {
    pub(super) checks: Vec<DoctorCheck>,
}

/// One `lev doctor` layer's verdict, reshaped for the browser: the enum
/// status becomes a plain `ok` bool so the client never parses labels.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct DoctorCheck {
    /// The layer's short name: `config`, `resolve`, `inference`, or `daemon`.
    pub(super) name: String,
    /// Whether the layer works. A failure is reported here, never as an
    /// HTTP error - the endpoint answering at all is not what is diagnosed.
    pub(super) ok: bool,
    pub(super) detail: String,
    /// Wall-clock cost, present for the checks that make a network call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) elapsed_ms: Option<u64>,
}

// ─── Filesystem types ───────────────────────────────────────────────────────

/// Query for `GET /api/fs/dirs`: the directory to list. Must be absolute when
/// given; absent means the server's own working directory (clamped to
/// `--workdir-root` when the cwd falls outside it).
#[derive(Deserialize)]
pub(super) struct DirsQuery {
    pub(super) path: Option<String>,
    /// Include dot-prefixed directories (hidden on Unix). Off by default so a
    /// first-run picker isn't a wall of config noise.
    #[serde(default)]
    pub(super) hidden: bool,
}

/// Response of `GET /api/fs/dirs`: one directory level of the host filesystem,
/// enough for the browser's folder picker to walk without shell access.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct DirsResp {
    /// The absolute directory that was listed.
    pub(super) path: String,
    /// Where "up one level" goes. `null` at the filesystem root, and also when
    /// `path` *is* the workdir-root - the picker is never led above the fence.
    pub(super) parent: Option<String>,
    /// The user's home directory, for the picker's "home" shortcut.
    pub(super) home: String,
    /// The serve process's working directory, for the picker's "here" shortcut.
    pub(super) cwd: String,
    /// The configured `--workdir-root`, or `null` when the server has none.
    pub(super) root: Option<String>,
    /// The immediate subdirectories, name-sorted. Dotted names are excluded,
    /// and with a root set, so is any symlink that resolves outside it.
    pub(super) dirs: Vec<DirEntry>,
}

/// One subdirectory in a [`DirsResp`] listing.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct DirEntry {
    pub(super) name: String,
    pub(super) path: String,
}

/// Body of `POST /api/fs/dirs`: make one directory inside another.
///
/// The parent and the new segment are separate fields rather than one joined
/// path, so the `--workdir-root` check runs on ground the caller has already
/// proved it can list, and a `name` carrying separators is malformed input
/// instead of something the fence has to catch.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct MkdirReq {
    /// The absolute directory to create it in. Must already exist.
    pub(super) path: String,
    /// The new directory's name: one segment, no separators, not `.` or `..`.
    pub(super) name: String,
}

/// Response of `POST /api/fs/dirs`: where the new directory landed.
#[derive(Debug, Serialize, Deserialize)]
pub(super) struct MkdirResp {
    /// The absolute path of the directory just created.
    pub(super) path: String,
    /// The directory it was created in, so a picker can re-list without
    /// re-deriving it from the path it was handed.
    pub(super) parent: String,
}

// ─── Tree types ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub(super) struct AgentTreeNode {
    pub(super) run_id: String,
    pub(super) agent_name: String,
    pub(super) status: String,
    pub(super) stage: String,
    pub(super) iteration: usize,
    pub(super) prompt_tokens: usize,
    pub(super) completion_tokens: usize,
    /// What this one agent spent, or `null` when some call it made could not be
    /// priced. Never `0` for unknown: a total that drops what it could not price
    /// reads as authoritative and understates.
    pub(super) cost_usd: Option<f64>,
    /// This agent and everything below it. `null` when anything in that subtree
    /// is unpriced, for the same reason.
    pub(super) subtree_cost_usd: Option<f64>,
    pub(super) children: Vec<AgentTreeNode>,
}

#[derive(Debug, Serialize)]
pub(super) struct TreeStatusNode {
    pub(super) run_id: String,
    pub(super) agent_name: String,
    pub(super) status: String,
    pub(super) stage: String,
    pub(super) prompt_tokens: usize,
    pub(super) completion_tokens: usize,
    pub(super) subtree_prompt_tokens: usize,
    pub(super) subtree_completion_tokens: usize,
    /// What this one agent spent, or `null` when some call it made could not be
    /// priced.
    pub(super) cost_usd: Option<f64>,
    /// This agent and everything below it, which is the number somebody asking
    /// "what did this run cost" means. `null` when anything in the subtree is
    /// unpriced.
    pub(super) subtree_cost_usd: Option<f64>,
    /// Calls in this subtree that carried no price. Non-zero is why
    /// `subtree_cost_usd` is `null`, and says how much of the total is missing.
    pub(super) subtree_unpriced_calls: usize,
    pub(super) children: Vec<TreeStatusNode>,
}

// ─── Interaction types ──────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(super) struct SubmitInteractionReq {
    pub(super) request_id: String,
    pub(super) value: Option<String>,
    pub(super) choice_index: Option<usize>,
    pub(super) approved: Option<bool>,
    pub(super) scope: Option<String>,
    /// On a deny, what the model should do instead. Optional, and absent is
    /// the plain deny every existing caller sends.
    #[serde(default)]
    pub(super) feedback: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct SendMessageReq {
    pub(super) message: String,
    #[serde(default)]
    pub(super) target_region: Option<String>,
}

// ─── Config types ───────────────────────────────────────────────────────────
//
// Moved to `config_types.rs` (this file was over the production-line limit) and
// re-exported here, so every `use super::types::*` still reaches them.
pub(super) use super::config_types::*;

#[derive(Serialize)]
pub(super) struct ModelEntry {
    pub(super) id: String,
    pub(super) provider: String,
    pub(super) display_name: Option<String>,
    pub(super) max_context_tokens: usize,
    pub(super) max_output_tokens: usize,
    /// Where the two limits above came from: `api`, `builtin` or `override`.
    ///
    /// Published because they are numbers a client acts on and the two kinds
    /// are not worth the same. `api` is what the provider says; `builtin` is
    /// this build matching the model's name against a compiled table, which is
    /// the only answer available for a provider whose API does not report
    /// limits at all, and is a guess that can be wrong by a factor of two.
    pub(super) limits_source: String,
    pub(super) supports_tools: bool,
    pub(super) supports_temperature: bool,
    /// Whether the provider's own listing described this model, as opposed
    /// to a row from the table compiled into this build.
    pub(super) learned: bool,
    /// When the provider released it, as Unix seconds, if its listing says.
    pub(super) released: Option<i64>,
    /// When the provider will withdraw it, as published, if its listing says.
    pub(super) retires: Option<String>,
    /// USD per million tokens, when the provider's listing quotes a rate.
    pub(super) pricing: Option<leviath_providers::ModelPricing>,
}

#[cfg(test)]
mod status_matches_tests {
    use super::*;
    use crate::runstate::RunStatus;

    /// The bug this function exists for: `WaitingInput` serializes as
    /// `waiting_input`, so that is the spelling a client has in hand - and the
    /// old `Display`-lowercased comparison rejected exactly that.
    #[test]
    fn the_serde_spelling_a_client_reads_back_is_accepted() {
        assert!(status_matches(&RunStatus::WaitingInput, "waiting_input"));
        assert!(status_matches(
            &RunStatus::CompleteInteractive,
            "complete_interactive"
        ));
    }

    /// The spellings that worked before must keep working - this widens the
    /// filter, it does not move it.
    #[test]
    fn the_display_spelling_that_already_worked_still_does() {
        assert!(status_matches(&RunStatus::WaitingInput, "waitinginput"));
        assert!(status_matches(&RunStatus::Running, "running"));
        assert!(status_matches(&RunStatus::Running, "Running"));
    }

    #[test]
    fn hyphens_and_mixed_case_are_accepted_too() {
        assert!(status_matches(&RunStatus::WaitingInput, "Waiting-Input"));
        assert!(status_matches(
            &RunStatus::CompleteInteractive,
            "COMPLETE-INTERACTIVE"
        ));
    }

    /// Normalizing must not collapse genuinely different statuses into each
    /// other, or a filter would quietly return the wrong runs.
    #[test]
    fn a_different_status_still_does_not_match() {
        assert!(!status_matches(&RunStatus::Running, "complete"));
        assert!(!status_matches(
            &RunStatus::Complete,
            "complete_interactive"
        ));
        assert!(!status_matches(&RunStatus::CompleteInteractive, "complete"));
        assert!(!status_matches(&RunStatus::Running, ""));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A daemon that is not there is a 503 (try later, or restart it); a
    /// daemon that answered in a way this server cannot read is a 502 with the
    /// remedy in the message, because no daemon restart fixes that.
    #[test]
    fn daemon_errors_are_503_unless_the_two_ends_no_longer_understand_each_other() {
        let (code, body) = daemon_error(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "no socket",
        ));
        assert_eq!(code, axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.error, "Daemon not reachable: no socket");

        let (code, body) = daemon_error(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the daemon is now version 9; restart this process",
        ));
        assert_eq!(code, axum::http::StatusCode::BAD_GATEWAY);
        assert_eq!(
            body.error,
            "This server needs a restart: the daemon is now version 9; restart this process"
        );
    }

    #[test]
    fn validate_response_serde_roundtrip() {
        let resp = ValidateResponse {
            valid: true,
            errors: None,
            warnings: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        // Neither list appears at all when there is nothing in it.
        assert_eq!(json, r#"{"valid":true}"#);
        let parsed: ValidateResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.valid);
        assert!(parsed.errors.is_none());
        assert!(parsed.warnings.is_none());
    }

    #[test]
    fn validate_response_with_errors_roundtrip() {
        let resp = ValidateResponse::invalid(vec!["bad field".to_string()]);
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: ValidateResponse = serde_json::from_str(&json).unwrap();
        assert!(!parsed.valid);
        assert_eq!(parsed.errors.unwrap().len(), 1);
        assert!(parsed.warnings.is_none());
    }

    /// A blueprint can be valid and still have something worth saying about it.
    #[test]
    fn validate_response_with_warnings_roundtrip() {
        let resp = ValidateResponse {
            valid: true,
            errors: None,
            warnings: Some(vec!["stage 'main': no max_iterations".to_string()]),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: ValidateResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.valid);
        assert_eq!(parsed.warnings.unwrap().len(), 1);
    }

    #[test]
    fn redacted_config_serde_roundtrip() {
        let config = RedactedConfig {
            default_provider: "anthropic".to_string(),
            has_anthropic_key: true,
            has_openai_key: false,
            has_google_key: false,
            has_openrouter_key: false,
            ollama_base_url: None,
            gateways: Vec::new(),
            agent_paths: vec![],
            mcp_server_count: 0,
            api_version: API_VERSION.to_string(),
            capabilities: API_CAPABILITIES.iter().map(|c| c.to_string()).collect(),
            limits: ApiLimits::current(&Default::default()),
            config_error: None,
            config_mtime: None,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: RedactedConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.default_provider, "anthropic");
        assert!(parsed.has_anthropic_key);
        assert!(!parsed.has_openai_key);
    }

    #[test]
    fn error_response_serialization() {
        let err = ErrorResponse {
            error: "not found".to_string(),
        };
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains("\"error\":\"not found\""));
    }

    #[test]
    fn file_content_resp_serde_roundtrip() {
        let resp = FileContentResp {
            path: "/work/report.md".to_string(),
            size: 9,
            offset: 0,
            next_offset: None,
            content: "# Report\n".to_string(),
            truncated: false,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: FileContentResp = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.path, "/work/report.md");
        assert_eq!(parsed.size, 9);
        assert_eq!(parsed.content, "# Report\n");
        assert!(!parsed.truncated);
    }

    #[test]
    fn dirs_resp_serde_roundtrip() {
        let resp = DirsResp {
            path: "/work".to_string(),
            parent: None,
            home: "/Users/someone".to_string(),
            cwd: "/work/project".to_string(),
            root: Some("/work".to_string()),
            dirs: vec![DirEntry {
                name: "src".to_string(),
                path: "/work/src".to_string(),
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        // An absent parent/root is an explicit `null`, never an omitted field -
        // the TypeScript client reads both unconditionally.
        assert!(json.contains("\"parent\":null"));
        assert!(json.contains(r#"{"name":"src","path":"/work/src"}"#));
        let parsed: DirsResp = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.path, "/work");
        assert!(parsed.parent.is_none());
        assert_eq!(parsed.root.as_deref(), Some("/work"));
        assert_eq!(parsed.dirs.len(), 1);
        assert_eq!(parsed.dirs[0].name, "src");
    }

    #[test]
    fn doctor_resp_serde_roundtrip() {
        let resp = DoctorResp {
            checks: vec![
                DoctorCheck {
                    name: "config".to_string(),
                    ok: true,
                    detail: "default_provider=anthropic".to_string(),
                    elapsed_ms: None,
                },
                DoctorCheck {
                    name: "inference".to_string(),
                    ok: false,
                    detail: "HTTP 401: bad key".to_string(),
                    elapsed_ms: Some(1200),
                },
            ],
        };
        let json = serde_json::to_string(&resp).unwrap();
        // An untimed check omits the field entirely rather than sending null.
        assert!(
            json.contains(r#"{"name":"config","ok":true,"detail":"default_provider=anthropic"}"#)
        );
        assert!(json.contains("\"elapsed_ms\":1200"));
        let parsed: DoctorResp = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.checks.len(), 2);
        assert!(parsed.checks[0].ok);
        assert!(parsed.checks[0].elapsed_ms.is_none());
        assert!(!parsed.checks[1].ok);
        assert_eq!(parsed.checks[1].elapsed_ms, Some(1200));
    }
}
