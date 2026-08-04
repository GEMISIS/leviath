//! Shared types: ServerEvent, AppState, request/response structs, error types.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Args;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::config::Config;

// ─── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Args)]
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

    /// Refuse `"yolo": true` on spawn requests, so an API caller cannot waive
    /// every approval prompt for an agent running on the host.
    #[arg(long)]
    pub no_remote_yolo: bool,
}

// ─── Shared state ────────────────────────────────────────────────────────────

/// Events broadcast to WebSocket subscribers.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    AgentStatus {
        agent_id: String,
        run_id: String,
        status: String,
        stage: String,
        iteration: usize,
        #[serde(default)]
        tool_calls: usize,
        accepts_messages: bool,
    },
    ContextUpdate {
        agent_id: String,
        run_id: String,
        total_tokens: usize,
        max_tokens: usize,
    },
    Log {
        agent_id: String,
        run_id: String,
        line: String,
    },
    InteractionNeeded {
        agent_id: String,
        run_id: String,
        request: serde_json::Value,
    },
    AgentSpawned {
        agent_id: String,
        run_id: String,
        parent_id: Option<String>,
        blueprint: String,
    },
    AgentCompleted {
        agent_id: String,
        run_id: String,
        status: String,
        /// The run's *error*, if it failed. Named `result` since before a run
        /// could produce one; kept for the consumers that read it.
        result: Option<String>,
        /// What the run handed back. This is the answer.
        #[serde(skip_serializing_if = "Option::is_none")]
        final_output: Option<FinalOutputResp>,
    },
    Tokens {
        agent_id: String,
        run_id: String,
        prompt_tokens: usize,
        completion_tokens: usize,
        #[serde(default)]
        cached_tokens: usize,
        #[serde(default)]
        cache_write_tokens: usize,
    },
    /// A world event with no dedicated WebSocket translation (stage
    /// transitions, tool call start/finish, and whatever the runtime adds
    /// next), forwarded verbatim. `event` is the runtime's own serde-tagged
    /// [`WorldEvent`](leviath_runtime::host::WorldEvent) JSON, so clients get
    /// new event kinds without a server release.
    World { event: serde_json::Value },
}

impl ServerEvent {
    /// The run id this event belongs to, for per-run subscription filtering.
    /// `World` events read it from the wrapped JSON (every runtime event
    /// carries one; an absent field filters as the empty string).
    pub fn run_id(&self) -> &str {
        match self {
            ServerEvent::AgentStatus { run_id, .. }
            | ServerEvent::ContextUpdate { run_id, .. }
            | ServerEvent::Log { run_id, .. }
            | ServerEvent::InteractionNeeded { run_id, .. }
            | ServerEvent::AgentSpawned { run_id, .. }
            | ServerEvent::AgentCompleted { run_id, .. }
            | ServerEvent::Tokens { run_id, .. } => run_id,
            ServerEvent::World { event } => event
                .get("run_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default(),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub(super) config: Arc<Config>,
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
    /// `--no-remote-yolo`: whether a spawn request may set `"yolo": true`.
    pub(super) no_remote_yolo: bool,
    /// `[security] allow_local_network`: whether a completion webhook may point
    /// at loopback, private or link-local addresses.
    pub(super) allow_local_network: bool,
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
        leviath_core::check_url(&parsed, self.allow_local_network)
            .map_err(|e| format!("callback_url is not allowed: {e}"))
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

/// Build a `(status, JSON error)` response tuple.
pub(super) fn err(
    code: axum::http::StatusCode,
    message: String,
) -> (axum::http::StatusCode, axum::response::Json<ErrorResponse>) {
    (code, axum::response::Json(ErrorResponse { error: message }))
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
pub struct FinalOutputResp {
    pub content: String,
    pub format: Option<String>,
    /// The stage that produced it.
    pub stage: String,
    /// Unix seconds at submission.
    pub submitted_at: i64,
    /// Whether the answer hit the size cap and was cut short.
    pub truncated: bool,
}

impl From<leviath_core::output::FinalOutput> for FinalOutputResp {
    fn from(o: leviath_core::output::FinalOutput) -> Self {
        Self {
            content: o.content,
            format: o.format,
            stage: o.stage,
            submitted_at: o.submitted_at,
            truncated: o.truncated,
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
    /// The file's bytes as UTF-8, capped at
    /// [`MAX_FILE_READ_BYTES`](super::agents::MAX_FILE_READ_BYTES).
    pub(super) content: String,
    /// Whether `content` is only the first
    /// [`MAX_FILE_READ_BYTES`](super::agents::MAX_FILE_READ_BYTES) of the file.
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
}

#[derive(Deserialize)]
pub(super) struct SendMessageReq {
    pub(super) message: String,
    #[serde(default)]
    pub(super) target_region: Option<String>,
}

// ─── Config types ───────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
pub(super) struct RedactedConfig {
    pub(super) default_provider: String,
    pub(super) has_anthropic_key: bool,
    pub(super) has_openai_key: bool,
    pub(super) has_google_key: bool,
    pub(super) has_openrouter_key: bool,
    pub(super) ollama_base_url: Option<String>,
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
    "logs.stage",
    "logs.stream",
    "context.history.page",
    "blueprints.envelope",
    "blueprints.query",
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
}

/// Body of `POST /api/config/validate` — a format-only key check (no network,
/// no persistence), mirroring the `lev setup` wizard's inline validation.
#[derive(Debug, Deserialize)]
pub(super) struct ValidateKeyReq {
    pub(super) provider: String,
    pub(super) key: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct ValidateKeyResp {
    pub(super) valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) message: Option<String>,
}

#[derive(Serialize)]
pub(super) struct ModelEntry {
    pub(super) id: String,
    pub(super) provider: String,
    pub(super) display_name: Option<String>,
    pub(super) max_context_tokens: usize,
    pub(super) max_output_tokens: usize,
    pub(super) supports_tools: bool,
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

    #[test]
    fn server_event_agent_status_serialization() {
        let event = ServerEvent::AgentStatus {
            agent_id: "coder".to_string(),
            run_id: "run-123".to_string(),
            status: "running".to_string(),
            stage: "implement".to_string(),
            iteration: 5,
            tool_calls: 12,
            accepts_messages: true,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"agent_status\""));
        assert!(json.contains("\"agent_id\":\"coder\""));
        assert!(json.contains("\"iteration\":5"));
        assert!(json.contains("\"tool_calls\":12"));
    }

    #[test]
    fn server_event_tokens_serialization() {
        let event = ServerEvent::Tokens {
            agent_id: "coder".to_string(),
            run_id: "run-123".to_string(),
            prompt_tokens: 5000,
            completion_tokens: 1200,
            cached_tokens: 200,
            cache_write_tokens: 100,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"tokens\""));
        assert!(json.contains("\"prompt_tokens\":5000"));
        assert!(json.contains("\"cached_tokens\":200"));
        assert!(json.contains("\"cache_write_tokens\":100"));
    }

    #[test]
    fn server_event_agent_spawned_serialization() {
        let event = ServerEvent::AgentSpawned {
            agent_id: "coder".to_string(),
            run_id: "run-456".to_string(),
            parent_id: Some("run-123".to_string()),
            blueprint: "coder".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"agent_spawned\""));
        assert!(json.contains("\"parent_id\":\"run-123\""));
    }

    #[test]
    fn server_event_agent_completed_serialization() {
        let event = ServerEvent::AgentCompleted {
            agent_id: "coder".to_string(),
            run_id: "run-123".to_string(),
            status: "complete".to_string(),
            result: Some("success".to_string()),
            final_output: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"agent_completed\""));
    }

    #[test]
    fn server_event_context_update_serialization() {
        let event = ServerEvent::ContextUpdate {
            agent_id: "coder".to_string(),
            run_id: "run-123".to_string(),
            total_tokens: 10000,
            max_tokens: 200000,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"context_update\""));
        assert!(json.contains("\"total_tokens\":10000"));
    }

    #[test]
    fn server_event_interaction_needed_serialization() {
        let event = ServerEvent::InteractionNeeded {
            agent_id: "coder".to_string(),
            run_id: "run-123".to_string(),
            request: serde_json::json!({"prompt": "approve?"}),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"interaction_needed\""));
    }

    #[test]
    fn server_event_log_serialization() {
        let event = ServerEvent::Log {
            agent_id: "coder".to_string(),
            run_id: "run-123".to_string(),
            line: "doing work".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"log\""));
        assert!(json.contains("\"line\":\"doing work\""));
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
            agent_paths: vec![],
            mcp_server_count: 0,
            api_version: API_VERSION.to_string(),
            capabilities: API_CAPABILITIES.iter().map(|c| c.to_string()).collect(),
            limits: ApiLimits::current(),
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

    #[test]
    fn server_event_run_id_covers_every_variant() {
        let cases: Vec<(ServerEvent, &str)> = vec![
            (
                ServerEvent::AgentStatus {
                    agent_id: "a".to_string(),
                    run_id: "r1".to_string(),
                    status: "active".to_string(),
                    stage: "s".to_string(),
                    iteration: 0,
                    tool_calls: 0,
                    accepts_messages: false,
                },
                "r1",
            ),
            (
                ServerEvent::ContextUpdate {
                    agent_id: "a".to_string(),
                    run_id: "r2".to_string(),
                    total_tokens: 1,
                    max_tokens: 2,
                },
                "r2",
            ),
            (
                ServerEvent::Log {
                    agent_id: "a".to_string(),
                    run_id: "r3".to_string(),
                    line: "l".to_string(),
                },
                "r3",
            ),
            (
                ServerEvent::InteractionNeeded {
                    agent_id: "a".to_string(),
                    run_id: "r4".to_string(),
                    request: serde_json::Value::Null,
                },
                "r4",
            ),
            (
                ServerEvent::AgentSpawned {
                    agent_id: "a".to_string(),
                    run_id: "r5".to_string(),
                    parent_id: None,
                    blueprint: "b".to_string(),
                },
                "r5",
            ),
            (
                ServerEvent::AgentCompleted {
                    agent_id: "a".to_string(),
                    run_id: "r6".to_string(),
                    status: "complete".to_string(),
                    result: None,
                    final_output: None,
                },
                "r6",
            ),
            (
                ServerEvent::Tokens {
                    agent_id: "a".to_string(),
                    run_id: "r7".to_string(),
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                },
                "r7",
            ),
            (
                ServerEvent::World {
                    event: serde_json::json!({"event": "stage_transition", "run_id": "r8"}),
                },
                "r8",
            ),
            // A wrapped event with no run_id filters as the empty string
            // rather than panicking (real world events always carry one).
            (
                ServerEvent::World {
                    event: serde_json::Value::Null,
                },
                "",
            ),
        ];
        for (ev, want) in cases {
            assert_eq!(ev.run_id(), want);
        }
    }
}
