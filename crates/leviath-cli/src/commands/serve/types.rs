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
        result: Option<String>,
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

// ─── Blueprint types ────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(super) struct BlueprintInfo {
    pub(super) name: String,
    pub(super) version: String,
    pub(super) description: String,
    pub(super) path: String,
    pub(super) stages: Vec<String>,
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

#[derive(Serialize)]
pub(super) struct AgentResultResp {
    pub(super) run_id: String,
    pub(super) status: String,
    pub(super) output: String,
    pub(super) error: Option<String>,
    pub(super) prompt_tokens: usize,
    pub(super) completion_tokens: usize,
}

#[derive(Deserialize)]
pub(super) struct LogsQuery {
    pub(super) tail: Option<u64>,
}

/// Query for `GET /api/agents/{id}/files`: the file to read. Relative paths
/// resolve against the run's workdir; absolute paths are accepted but must
/// still land inside it.
#[derive(Deserialize)]
pub(super) struct FileQuery {
    pub(super) path: String,
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
