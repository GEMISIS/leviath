//! Agent Client Protocol wire types.
//!
//! JSON-RPC 2.0 messages, newline-delimited, one compact message per line. Field
//! names are camelCase and discriminator *values* are snake_case, per the spec.
//!
//! Every type a client sends us is permissive on deserialize — unknown fields are
//! ignored and every optional field carries `#[serde(default)]` — because real
//! hosts differ in how much of the spec they populate. Gas City, for instance,
//! sends `initialize` with only `protocolVersion` and `clientInfo` and no
//! `clientCapabilities` at all.

use serde::{Deserialize, Serialize};

/// The protocol MAJOR version this crate speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// The largest single JSON frame (one line) we will ever write.
///
/// Hosts read the stdio stream line-by-line with a bounded buffer and drop —
/// or error on — anything longer: Gas City's `bufio.Scanner` is capped at 1 MiB
/// and abandons its read loop on an oversized frame, which would strand the
/// session. Callers streaming agent output must therefore split it into chunks
/// no larger than this, leaving generous headroom for JSON escaping (a
/// worst-case string of control characters expands ~6×).
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// Compile-time guard on the headroom [`MAX_FRAME_BYTES`] claims: even if every
/// byte of a chunk needed the longest JSON escape (`\u00XX`, 6×), the frame must
/// still fit inside a host's 1 MiB line limit.
const _: () = assert!(MAX_FRAME_BYTES * 6 < 1024 * 1024);

/// Standard JSON-RPC 2.0 error codes.
pub mod error_codes {
    /// Invalid JSON was received.
    pub const PARSE_ERROR: i32 = -32700;
    /// The JSON sent is not a valid request object.
    pub const INVALID_REQUEST: i32 = -32600;
    /// The method does not exist.
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// Invalid method parameters.
    pub const INVALID_PARAMS: i32 = -32602;
    /// Internal agent error.
    pub const INTERNAL_ERROR: i32 = -32603;
}

// ─── Envelope ────────────────────────────────────────────────────────────────

/// A JSON-RPC 2.0 message: request, response, or notification depending on which
/// fields are set.
///
/// `id` is a [`serde_json::Value`] rather than an integer so any id a host uses
/// (number or string, both legal) round-trips back verbatim in the response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcMessage {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Request/response correlation id; absent on notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    /// Method name; set on requests and notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Method parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    /// Success payload; set on responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Failure payload; set on error responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcMessage {
    /// A successful response to the request identified by `id`.
    ///
    /// `result` is always one of this module's protocol types, whose
    /// serialization is infallible; a failure would be a bug here rather than a
    /// runtime condition, so it degrades to JSON `null` instead of propagating.
    pub fn response(id: serde_json::Value, result: &impl Serialize) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Some(id),
            method: None,
            params: None,
            result: Some(serde_json::to_value(result).unwrap_or(serde_json::Value::Null)),
            error: None,
        }
    }

    /// An error response to the request identified by `id`.
    pub fn error_response(id: serde_json::Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Some(id),
            method: None,
            params: None,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
            }),
        }
    }

    /// An outbound notification (no id, no response expected).
    pub fn notification(method: impl Into<String>, params: &impl Serialize) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: Some(method.into()),
            params: Some(serde_json::to_value(params).unwrap_or(serde_json::Value::Null)),
            result: None,
            error: None,
        }
    }

    /// An outbound request (agent → client), e.g. `session/request_permission`.
    pub fn request(
        id: serde_json::Value,
        method: impl Into<String>,
        params: &impl Serialize,
    ) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Some(id),
            method: Some(method.into()),
            params: Some(serde_json::to_value(params).unwrap_or(serde_json::Value::Null)),
            result: None,
            error: None,
        }
    }

    /// Whether this message is a notification: a method call with no id, which
    /// must never be answered.
    pub fn is_notification(&self) -> bool {
        self.id.is_none() && self.method.is_some()
    }
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// The error code (see [`error_codes`]).
    pub code: i32,
    /// A human-readable description.
    pub message: String,
}

// ─── Content ─────────────────────────────────────────────────────────────────

/// One block of prompt or message content.
///
/// Modelled as a permissive struct rather than a tagged enum: hosts send block
/// kinds we do not advertise support for (`image`, `audio`, `resource_link`),
/// and a strict enum would fail the whole prompt rather than skipping the block
/// we cannot use. Unknown kinds deserialize with `text`/`resource` both `None`
/// and are dropped by [`crate::flatten_prompt`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentBlock {
    /// The block kind: `text`, `resource`, `image`, `audio`, `resource_link`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The text, for `text` blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The inlined resource, for `resource` blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<EmbeddedResource>,
}

impl ContentBlock {
    /// A `text` content block.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            kind: "text".to_string(),
            text: Some(text.into()),
            resource: None,
        }
    }
}

/// A resource inlined into a prompt (the `embeddedContext` capability).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddedResource {
    /// The resource's URI.
    pub uri: String,
    /// The resource's MIME type, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// The resource's textual content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

// ─── initialize ──────────────────────────────────────────────────────────────

/// Params of the `initialize` request.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// The MAJOR protocol version the client speaks.
    #[serde(default)]
    pub protocol_version: u32,
    /// What the client can do on the agent's behalf. Absent for hosts that do
    /// not implement the client-side methods at all.
    #[serde(default)]
    pub client_capabilities: Option<ClientCapabilities>,
}

/// Client-side capabilities advertised at `initialize`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    /// Filesystem methods the client offers (`fs/read_text_file` etc.).
    #[serde(default)]
    pub fs: Option<serde_json::Value>,
    /// Whether the client offers the `terminal/*` methods.
    #[serde(default)]
    pub terminal: Option<bool>,
}

/// Result of the `initialize` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    /// The MAJOR protocol version the agent speaks.
    pub protocol_version: u32,
    /// What this agent supports.
    pub agent_capabilities: AgentCapabilities,
    /// This agent's identity.
    pub agent_info: AgentInfo,
    /// Authentication methods; Leviath authenticates against LLM providers
    /// itself, so this is always empty.
    pub auth_methods: Vec<serde_json::Value>,
}

/// Agent-side capabilities advertised at `initialize`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    /// Whether `session/load` is supported (resuming a session by id).
    pub load_session: bool,
    /// Which prompt content kinds the agent accepts.
    pub prompt_capabilities: PromptCapabilities,
}

/// Prompt content kinds an agent accepts.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptCapabilities {
    /// `image` content blocks.
    pub image: bool,
    /// `audio` content blocks.
    pub audio: bool,
    /// `resource` content blocks with inlined text.
    pub embedded_context: bool,
}

/// An agent's identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    /// Machine-readable name.
    pub name: String,
    /// Version string.
    pub version: String,
}

// ─── session/new ─────────────────────────────────────────────────────────────

/// Params of the `session/new` request.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNewParams {
    /// Absolute working directory for the session.
    #[serde(default)]
    pub cwd: String,
    /// MCP servers the client wants attached. Captured verbatim — Leviath
    /// blueprints declare their own MCP servers, so these are logged and not
    /// injected (see the module docs of `leviath_cli::commands::agent_client`).
    #[serde(default)]
    pub mcp_servers: Vec<serde_json::Value>,
}

/// Result of the `session/new` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNewResult {
    /// The new session's id.
    pub session_id: String,
}

// ─── session/prompt ──────────────────────────────────────────────────────────

/// Params of the `session/prompt` request.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPromptParams {
    /// The session to prompt.
    #[serde(default)]
    pub session_id: String,
    /// The prompt's content blocks.
    #[serde(default)]
    pub prompt: Vec<ContentBlock>,
}

/// Result of the `session/prompt` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPromptResult {
    /// Why the turn ended.
    pub stop_reason: StopReason,
}

/// Why a prompt turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The agent finished its turn normally.
    EndTurn,
    /// The token limit was reached.
    MaxTokens,
    /// The per-turn model-request limit was exceeded.
    MaxTurnRequests,
    /// The agent declined to continue.
    Refusal,
    /// The client cancelled the turn.
    Cancelled,
}

// ─── session/cancel ──────────────────────────────────────────────────────────

/// Params of the `session/cancel` notification.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCancelParams {
    /// The session whose in-flight turn should be cancelled.
    #[serde(default)]
    pub session_id: String,
}

// ─── session/update ──────────────────────────────────────────────────────────

/// Params of an outbound `session/update` notification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUpdateParams {
    /// The session the update belongs to.
    pub session_id: String,
    /// The update itself.
    pub update: SessionUpdate,
}

/// One `session/update` payload, discriminated by `sessionUpdate`.
///
/// Only the variants Leviath emits are modelled. The spec also defines
/// `agent_thought_chunk`, `tool_call`, `tool_call_update`, `plan`,
/// `available_commands_update` and `current_mode_update`; adding one is a
/// matter of adding a variant here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    /// A chunk of the agent's user-visible output.
    AgentMessageChunk {
        /// The chunk's content.
        content: ContentBlock,
    },
    /// Context-window consumption, for host-side progress display.
    #[serde(rename_all = "camelCase")]
    UsageUpdate {
        /// Context tokens currently in use.
        used: usize,
        /// The context window's capacity.
        size: usize,
    },
}

// ─── session/request_permission ──────────────────────────────────────────────

/// Params of an outbound `session/request_permission` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionParams {
    /// The session the tool call belongs to.
    pub session_id: String,
    /// The tool call awaiting approval.
    pub tool_call: ToolCallRef,
    /// The choices offered to the user.
    pub options: Vec<PermissionOption>,
}

/// The tool call a permission request refers to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallRef {
    /// Correlation id for this tool call.
    pub tool_call_id: String,
    /// Human-readable summary.
    pub title: String,
    /// What kind of operation it is.
    pub kind: ToolKind,
    /// Its current status.
    pub status: ToolCallStatus,
}

/// A tool call's lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    /// Not started (e.g. awaiting permission).
    Pending,
    /// Executing.
    InProgress,
    /// Finished successfully.
    Completed,
    /// Finished with an error.
    Failed,
}

/// The category of operation a tool performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// Reads data.
    Read,
    /// Modifies data.
    Edit,
    /// Deletes data.
    Delete,
    /// Moves or renames data.
    Move,
    /// Searches.
    Search,
    /// Executes a command.
    Execute,
    /// Reasons without side effects.
    Think,
    /// Fetches remote data.
    Fetch,
    /// Anything else.
    Other,
}

/// One choice offered in a permission request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    /// The id echoed back in the outcome.
    pub option_id: String,
    /// The label shown to the user.
    pub name: String,
    /// What selecting it means.
    pub kind: PermissionOptionKind,
}

/// What selecting a permission option means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    /// Approve this call only.
    AllowOnce,
    /// Approve this and future equivalent calls.
    AllowAlways,
    /// Deny this call only.
    RejectOnce,
    /// Deny this and future equivalent calls.
    RejectAlways,
}

/// Result of a `session/request_permission` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestPermissionResult {
    /// What the user chose.
    pub outcome: PermissionOutcome,
}

/// The user's decision on a permission request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PermissionOutcome {
    /// An option was chosen.
    #[serde(rename_all = "camelCase")]
    Selected {
        /// The chosen option's id.
        option_id: String,
    },
    /// The turn was cancelled before the user chose.
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize `value` compactly, as the wire framing does.
    fn json(value: &impl Serialize) -> String {
        serde_json::to_string(value).unwrap()
    }

    /// Serialize `value` and re-parse it, for comparing against an expected
    /// shape without pinning field *order*.
    ///
    /// Whole-[`JsonRpcMessage`] assertions must go through this: `params` and
    /// `result` are stored as [`serde_json::Value`], whose object map is sorted,
    /// so the emitted key order is not the declaration order. JSON objects are
    /// unordered by definition and every host parses by name, so this is a
    /// property of the encoding rather than something to assert on.
    fn shape(value: &impl Serialize) -> serde_json::Value {
        serde_json::from_str(&json(value)).unwrap()
    }

    #[test]
    fn response_carries_id_and_result_and_omits_everything_else() {
        let msg = JsonRpcMessage::response(
            serde_json::json!(7),
            &SessionNewResult {
                session_id: "s1".to_string(),
            },
        );
        assert_eq!(
            json(&msg),
            r#"{"jsonrpc":"2.0","id":7,"result":{"sessionId":"s1"}}"#
        );
        assert!(!msg.is_notification());
    }

    #[test]
    fn error_response_carries_code_and_message() {
        let msg = JsonRpcMessage::error_response(
            serde_json::json!("abc"),
            error_codes::METHOD_NOT_FOUND,
            "no such method",
        );
        assert_eq!(
            json(&msg),
            r#"{"jsonrpc":"2.0","id":"abc","error":{"code":-32601,"message":"no such method"}}"#
        );
        assert!(!msg.is_notification());
    }

    #[test]
    fn notification_has_no_id() {
        let msg = JsonRpcMessage::notification(
            "session/update",
            &SessionUpdateParams {
                session_id: "s1".to_string(),
                update: SessionUpdate::AgentMessageChunk {
                    content: ContentBlock::text("hi"),
                },
            },
        );
        assert_eq!(
            shape(&msg),
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "s1",
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": {"type": "text", "text": "hi"},
                    },
                },
            })
        );
        // No `id` key at all — a notification must never invite a response.
        assert!(!json(&msg).contains("\"id\""));
        assert!(msg.is_notification());
    }

    #[test]
    fn request_has_both_id_and_method() {
        let msg = JsonRpcMessage::request(
            serde_json::json!(1),
            "session/request_permission",
            &serde_json::json!({"sessionId": "s1"}),
        );
        assert_eq!(
            json(&msg),
            r#"{"jsonrpc":"2.0","id":1,"method":"session/request_permission","params":{"sessionId":"s1"}}"#
        );
        assert!(!msg.is_notification());
    }

    #[test]
    fn a_response_with_neither_id_nor_method_is_not_a_notification() {
        let msg: JsonRpcMessage = serde_json::from_str(r#"{"jsonrpc":"2.0"}"#).unwrap();
        assert!(!msg.is_notification());
    }

    #[test]
    fn usage_update_uses_camel_case_fields() {
        let msg = JsonRpcMessage::notification(
            "session/update",
            &SessionUpdateParams {
                session_id: "s1".to_string(),
                update: SessionUpdate::UsageUpdate {
                    used: 10,
                    size: 200,
                },
            },
        );
        assert_eq!(
            shape(&msg)["params"]["update"],
            serde_json::json!({"sessionUpdate": "usage_update", "used": 10, "size": 200})
        );
    }

    #[test]
    fn session_update_round_trips() {
        let update = SessionUpdate::AgentMessageChunk {
            content: ContentBlock::text("out"),
        };
        assert_eq!(
            serde_json::from_str::<SessionUpdate>(&json(&update)).unwrap(),
            update
        );
        let usage = SessionUpdate::UsageUpdate { used: 1, size: 2 };
        assert_eq!(
            serde_json::from_str::<SessionUpdate>(&json(&usage)).unwrap(),
            usage
        );
    }

    #[test]
    fn initialize_params_tolerate_a_bare_protocol_version() {
        // Gas City sends `protocolVersion` + `clientInfo` and no capabilities.
        let params: InitializeParams = serde_json::from_str(
            r#"{"protocolVersion":1,"clientInfo":{"name":"gc","version":"1.0"}}"#,
        )
        .unwrap();
        assert_eq!(params.protocol_version, 1);
        assert!(params.client_capabilities.is_none());

        // A fully-populated client round-trips too.
        let full: InitializeParams = serde_json::from_str(
            r#"{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":true},"terminal":true}}"#,
        )
        .unwrap();
        let caps = full.client_capabilities.unwrap();
        assert!(caps.terminal.unwrap());
        assert!(caps.fs.is_some());

        // Entirely absent params still deserialize.
        assert_eq!(
            serde_json::from_str::<InitializeParams>("{}").unwrap(),
            InitializeParams::default()
        );
    }

    #[test]
    fn initialize_result_serializes_the_spec_shape() {
        let result = InitializeResult {
            protocol_version: PROTOCOL_VERSION,
            agent_capabilities: AgentCapabilities {
                load_session: false,
                prompt_capabilities: PromptCapabilities {
                    image: false,
                    audio: false,
                    embedded_context: true,
                },
            },
            agent_info: AgentInfo {
                name: "leviath".to_string(),
                version: "0.1.0".to_string(),
            },
            auth_methods: vec![],
        };
        assert_eq!(
            json(&result),
            r#"{"protocolVersion":1,"agentCapabilities":{"loadSession":false,"promptCapabilities":{"image":false,"audio":false,"embeddedContext":true}},"agentInfo":{"name":"leviath","version":"0.1.0"},"authMethods":[]}"#
        );
        assert_eq!(
            serde_json::from_str::<InitializeResult>(&json(&result)).unwrap(),
            result
        );
    }

    #[test]
    fn session_new_params_default_every_field() {
        let params: SessionNewParams = serde_json::from_str("{}").unwrap();
        assert_eq!(params, SessionNewParams::default());
        assert_eq!(params.cwd, "");
        assert!(params.mcp_servers.is_empty());

        let populated: SessionNewParams =
            serde_json::from_str(r#"{"cwd":"/w","mcpServers":[{"name":"x"}]}"#).unwrap();
        assert_eq!(populated.cwd, "/w");
        assert_eq!(populated.mcp_servers.len(), 1);
        // Round-trips, so the captured servers can be logged verbatim.
        assert_eq!(
            serde_json::from_str::<SessionNewParams>(&json(&populated)).unwrap(),
            populated
        );
    }

    #[test]
    fn prompt_params_accept_unknown_block_kinds() {
        let params: SessionPromptParams = serde_json::from_str(
            r#"{"sessionId":"s","prompt":[{"type":"text","text":"hi"},{"type":"image","data":"..."}]}"#,
        )
        .unwrap();
        assert_eq!(params.session_id, "s");
        assert_eq!(params.prompt.len(), 2);
        assert_eq!(params.prompt[1].kind, "image");
        assert!(params.prompt[1].text.is_none());
        assert!(params.prompt[1].resource.is_none());

        assert_eq!(
            serde_json::from_str::<SessionPromptParams>("{}").unwrap(),
            SessionPromptParams::default()
        );
    }

    #[test]
    fn embedded_resource_round_trips_with_and_without_optionals() {
        let full = EmbeddedResource {
            uri: "file:///a.rs".to_string(),
            mime_type: Some("text/rust".to_string()),
            text: Some("fn main() {}".to_string()),
        };
        assert_eq!(
            json(&full),
            r#"{"uri":"file:///a.rs","mimeType":"text/rust","text":"fn main() {}"}"#
        );
        assert_eq!(
            serde_json::from_str::<EmbeddedResource>(&json(&full)).unwrap(),
            full
        );

        let bare = EmbeddedResource {
            uri: "u".to_string(),
            mime_type: None,
            text: None,
        };
        assert_eq!(json(&bare), r#"{"uri":"u"}"#);
    }

    #[test]
    fn content_block_text_constructor_and_round_trip() {
        let block = ContentBlock::text("hello");
        assert_eq!(json(&block), r#"{"type":"text","text":"hello"}"#);
        assert_eq!(
            serde_json::from_str::<ContentBlock>(&json(&block)).unwrap(),
            block
        );

        let resource = ContentBlock {
            kind: "resource".to_string(),
            text: None,
            resource: Some(EmbeddedResource {
                uri: "u".to_string(),
                mime_type: None,
                text: Some("body".to_string()),
            }),
        };
        assert_eq!(
            serde_json::from_str::<ContentBlock>(&json(&resource)).unwrap(),
            resource
        );
    }

    #[test]
    fn stop_reasons_use_snake_case() {
        for (reason, wire) in [
            (StopReason::EndTurn, r#""end_turn""#),
            (StopReason::MaxTokens, r#""max_tokens""#),
            (StopReason::MaxTurnRequests, r#""max_turn_requests""#),
            (StopReason::Refusal, r#""refusal""#),
            (StopReason::Cancelled, r#""cancelled""#),
        ] {
            assert_eq!(json(&reason), wire);
            assert_eq!(serde_json::from_str::<StopReason>(wire).unwrap(), reason);
        }
        assert_eq!(
            json(&SessionPromptResult {
                stop_reason: StopReason::EndTurn
            }),
            r#"{"stopReason":"end_turn"}"#
        );
        assert_eq!(
            serde_json::from_str::<SessionPromptResult>(r#"{"stopReason":"refusal"}"#)
                .unwrap()
                .stop_reason,
            StopReason::Refusal
        );
    }

    #[test]
    fn session_cancel_params_default_the_session_id() {
        assert_eq!(
            serde_json::from_str::<SessionCancelParams>("{}").unwrap(),
            SessionCancelParams::default()
        );
        let params: SessionCancelParams = serde_json::from_str(r#"{"sessionId":"s"}"#).unwrap();
        assert_eq!(params.session_id, "s");
        assert_eq!(json(&params), r#"{"sessionId":"s"}"#);
    }

    #[test]
    fn permission_request_serializes_the_spec_shape() {
        let params = RequestPermissionParams {
            session_id: "s1".to_string(),
            tool_call: ToolCallRef {
                tool_call_id: "t1".to_string(),
                title: "run tests".to_string(),
                kind: ToolKind::Execute,
                status: ToolCallStatus::Pending,
            },
            options: vec![PermissionOption {
                option_id: "allow-once".to_string(),
                name: "Allow".to_string(),
                kind: PermissionOptionKind::AllowOnce,
            }],
        };
        assert_eq!(
            json(&params),
            r#"{"sessionId":"s1","toolCall":{"toolCallId":"t1","title":"run tests","kind":"execute","status":"pending"},"options":[{"optionId":"allow-once","name":"Allow","kind":"allow_once"}]}"#
        );
        assert_eq!(
            serde_json::from_str::<RequestPermissionParams>(&json(&params)).unwrap(),
            params
        );
    }

    #[test]
    fn every_tool_and_permission_enum_value_round_trips() {
        for (kind, wire) in [
            (ToolKind::Read, r#""read""#),
            (ToolKind::Edit, r#""edit""#),
            (ToolKind::Delete, r#""delete""#),
            (ToolKind::Move, r#""move""#),
            (ToolKind::Search, r#""search""#),
            (ToolKind::Execute, r#""execute""#),
            (ToolKind::Think, r#""think""#),
            (ToolKind::Fetch, r#""fetch""#),
            (ToolKind::Other, r#""other""#),
        ] {
            assert_eq!(json(&kind), wire);
            assert_eq!(serde_json::from_str::<ToolKind>(wire).unwrap(), kind);
        }
        for (status, wire) in [
            (ToolCallStatus::Pending, r#""pending""#),
            (ToolCallStatus::InProgress, r#""in_progress""#),
            (ToolCallStatus::Completed, r#""completed""#),
            (ToolCallStatus::Failed, r#""failed""#),
        ] {
            assert_eq!(json(&status), wire);
            assert_eq!(
                serde_json::from_str::<ToolCallStatus>(wire).unwrap(),
                status
            );
        }
        for (kind, wire) in [
            (PermissionOptionKind::AllowOnce, r#""allow_once""#),
            (PermissionOptionKind::AllowAlways, r#""allow_always""#),
            (PermissionOptionKind::RejectOnce, r#""reject_once""#),
            (PermissionOptionKind::RejectAlways, r#""reject_always""#),
        ] {
            assert_eq!(json(&kind), wire);
            assert_eq!(
                serde_json::from_str::<PermissionOptionKind>(wire).unwrap(),
                kind
            );
        }
    }

    #[test]
    fn permission_outcomes_round_trip() {
        let selected = RequestPermissionResult {
            outcome: PermissionOutcome::Selected {
                option_id: "allow-once".to_string(),
            },
        };
        assert_eq!(
            json(&selected),
            r#"{"outcome":{"outcome":"selected","optionId":"allow-once"}}"#
        );
        assert_eq!(
            serde_json::from_str::<RequestPermissionResult>(&json(&selected)).unwrap(),
            selected
        );

        let cancelled = RequestPermissionResult {
            outcome: PermissionOutcome::Cancelled,
        };
        assert_eq!(json(&cancelled), r#"{"outcome":{"outcome":"cancelled"}}"#);
        assert_eq!(
            serde_json::from_str::<RequestPermissionResult>(&json(&cancelled)).unwrap(),
            cancelled
        );
    }

    #[test]
    fn agent_capability_defaults_are_all_false() {
        assert_eq!(
            json(&AgentCapabilities::default()),
            r#"{"loadSession":false,"promptCapabilities":{"image":false,"audio":false,"embeddedContext":false}}"#
        );
    }
}
