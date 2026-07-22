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

    /// Allow CORS from origin (default: *)
    #[arg(long, default_value = "*")]
    pub cors: String,

    /// API token clients must present (`Authorization: Bearer <token>`, or
    /// `?token=` for WebSockets). Overrides the LEVIATH_API_TOKEN env var; the
    /// server refuses to start if neither is set.
    #[arg(long)]
    pub token: Option<String>,
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
}

// ─── Agent types ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
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
    pub(super) workdir: Option<String>,
    #[serde(default)]
    pub(super) metadata: HashMap<String, String>,
    pub(super) callback_url: Option<String>,
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
    pub(super) has_openrouter_key: bool,
    pub(super) ollama_base_url: Option<String>,
    pub(super) agent_paths: Vec<PathBuf>,
    pub(super) registries: Vec<String>,
    pub(super) mcp_server_count: usize,
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
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: ValidateResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.valid);
        assert!(parsed.errors.is_none());
    }

    #[test]
    fn validate_response_with_errors_roundtrip() {
        let resp = ValidateResponse {
            valid: false,
            errors: Some(vec!["bad field".to_string()]),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: ValidateResponse = serde_json::from_str(&json).unwrap();
        assert!(!parsed.valid);
        assert_eq!(parsed.errors.unwrap().len(), 1);
    }

    #[test]
    fn redacted_config_serde_roundtrip() {
        let config = RedactedConfig {
            default_provider: "anthropic".to_string(),
            has_anthropic_key: true,
            has_openai_key: false,
            has_openrouter_key: false,
            ollama_base_url: None,
            agent_paths: vec![],
            registries: vec![],
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
}
