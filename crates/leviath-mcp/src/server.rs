//! The server side of the MCP wire: the result shapes a host reads back.
//!
//! Everything here is a pure function over plain values. The stdio loop, the
//! daemon bridge and the tool table live in `leviath-cli` (`lev mcp serve`);
//! this module only knows what an `initialize` result, a `tools/list` entry, a
//! `tools/call` result and a progress notification look like, so those shapes
//! are written once and tested without a socket.
//!
//! The dialect is the legacy `initialize` handshake every current host speaks
//! (Claude Code, Grok Build, Codex, Gemini, Hermes): the server echoes the
//! client's protocol revision when it is one this crate knows, and otherwise
//! answers with the revision it prefers, which the hosts' SDKs accept.

use serde::Serialize;
use serde_json::{Value, json};

use crate::client::{PREFERRED_PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS};
pub use crate::transport::jsonrpc::METHOD_NOT_FOUND;

/// The server name hosts key tools by (`mcp__leviath__run`, `leviath__run`,
/// `mcp_leviath_run`). One lowercase word: every host folds it into the tool
/// name the model sees, and Gemini truncates the result at 63 characters.
pub const SERVER_NAME: &str = "leviath";

/// The human title shown beside [`SERVER_NAME`] in a host's server list.
pub const SERVER_TITLE: &str = "Leviath";

/// How a tool behaves, in the vocabulary hosts use to decide whether to ask
/// the user before each call. Claude Code and Grok Build both read these:
/// without `readOnlyHint` on a status poll, every poll can raise a prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
    /// The tool changes nothing.
    pub read_only_hint: bool,
    /// The tool may destroy something that cannot be recovered.
    pub destructive_hint: bool,
    /// Calling the tool twice with the same arguments has the effect of once.
    pub idempotent_hint: bool,
    /// The tool reaches outside the host's closed world (spawns work, writes
    /// files the host does not see).
    pub open_world_hint: bool,
}

impl ToolAnnotations {
    /// A tool that only reads.
    pub const READ_ONLY: Self = Self {
        read_only_hint: true,
        destructive_hint: false,
        idempotent_hint: true,
        open_world_hint: false,
    };

    /// A tool that starts or changes something outside the host.
    pub const OPEN_WORLD: Self = Self {
        read_only_hint: false,
        destructive_hint: false,
        idempotent_hint: false,
        open_world_hint: true,
    };

    /// A tool that stops or removes something.
    pub const DESTRUCTIVE: Self = Self {
        read_only_hint: false,
        destructive_hint: true,
        idempotent_hint: true,
        open_world_hint: false,
    };

    /// A tool that sends something to a run without reading or destroying.
    pub const MUTATING: Self = Self {
        read_only_hint: false,
        destructive_hint: false,
        idempotent_hint: false,
        open_world_hint: false,
    };
}

/// One entry of a `tools/list` result.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerTool {
    /// The name the host calls the tool by (before it prefixes the server).
    pub name: String,
    /// A short human title.
    pub title: String,
    /// What the tool does and when to call it, shown to the model verbatim.
    pub description: String,
    /// The JSON Schema of `arguments`.
    pub input_schema: Value,
    /// Behaviour hints; see [`ToolAnnotations`].
    pub annotations: ToolAnnotations,
}

/// The `initialize` result.
///
/// `client_version` is the `protocolVersion` the client offered. It is echoed
/// when this crate knows it; otherwise the preferred revision is answered,
/// which is what the hosts' SDKs do on their side too. `instructions` reaches
/// the host model as the server's standing instructions (Claude Code shows it
/// under "MCP Server Instructions").
pub fn initialize_result(client_version: Option<&str>, instructions: &str) -> Value {
    let version = client_version
        .filter(|v| SUPPORTED_PROTOCOL_VERSIONS.contains(v))
        .unwrap_or(PREFERRED_PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": SERVER_NAME,
            "title": SERVER_TITLE,
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": instructions,
    })
}

/// The `tools/list` result for `tools`. No `nextCursor`: the table is small
/// and fixed, so it is always sent whole.
pub fn tool_list_result(tools: &[ServerTool]) -> Value {
    json!({ "tools": tools })
}

/// A `tools/call` result carrying one text block.
///
/// `is_error` marks a tool that ran and failed (a refused spawn, a run that
/// ended in error), which the model reads and corrects for; a request the
/// server could not even attempt (an unknown tool, a malformed argument) is a
/// JSON-RPC error instead, and never comes through here. `structured` is the
/// same answer as data, for hosts that surface `structuredContent`.
pub fn text_result(text: impl Into<String>, is_error: bool, structured: Option<Value>) -> Value {
    let mut result = json!({
        "content": [{ "type": "text", "text": text.into() }],
        "isError": is_error,
    });
    if let Some(structured) = structured {
        result["structuredContent"] = structured;
    }
    result
}

/// The `ping` result: an empty object, as the protocol requires.
pub fn ping_result() -> Value {
    json!({})
}

/// The error a request for a method this server does not implement gets.
///
/// Wording shared with the client side's reply to a server-initiated request,
/// so both directions of Leviath's MCP surface refuse the same way. Answering
/// `server/discover` with this is what lets a client of the 2026-07-28
/// revision fall back to the `initialize` handshake.
pub fn method_not_found(method: &str) -> (i32, String) {
    (
        METHOD_NOT_FOUND,
        format!("Leviath does not implement '{method}'"),
    )
}

/// The params of a `notifications/progress` for the call that carried
/// `progress_token`.
///
/// `progress` must increase on every notification for one token, and the
/// token must be the one the client put in the call's `_meta`: a host matches
/// it against its outstanding requests and only resets its idle timer on a
/// match. No `total`, because a run's length is not known in advance.
/// `message` was added to the notification in the 2025-03-26 revision; a
/// 2024-11-05 client ignores it.
pub fn progress_params(progress_token: &Value, progress: u64, message: &str) -> Value {
    json!({
        "progressToken": progress_token,
        "progress": progress,
        "message": message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_echoes_a_supported_client_version_and_carries_the_instructions() {
        let result = initialize_result(Some("2024-11-05"), "Say leviath.");
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
        assert_eq!(result["serverInfo"]["name"], SERVER_NAME);
        assert_eq!(result["serverInfo"]["title"], SERVER_TITLE);
        assert_eq!(result["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(result["instructions"], "Say leviath.");
    }

    #[test]
    fn initialize_answers_the_preferred_version_to_an_unknown_or_absent_one() {
        assert_eq!(
            initialize_result(Some("2099-01-01"), "")["protocolVersion"],
            PREFERRED_PROTOCOL_VERSION
        );
        assert_eq!(
            initialize_result(None, "")["protocolVersion"],
            PREFERRED_PROTOCOL_VERSION
        );
    }

    #[test]
    fn a_tool_list_serializes_camel_case_schema_and_annotations() {
        let tools = vec![ServerTool {
            name: "status".to_string(),
            title: "Run status".to_string(),
            description: "Where a run stands.".to_string(),
            input_schema: json!({ "type": "object", "properties": { "run_id": { "type": "string" } } }),
            annotations: ToolAnnotations::READ_ONLY,
        }];
        let listed = tool_list_result(&tools);
        let entry = &listed["tools"][0];
        assert_eq!(entry["name"], "status");
        assert_eq!(entry["title"], "Run status");
        assert_eq!(entry["inputSchema"]["type"], "object");
        assert_eq!(entry["annotations"]["readOnlyHint"], true);
        assert_eq!(entry["annotations"]["destructiveHint"], false);
        assert_eq!(entry["annotations"]["idempotentHint"], true);
        assert_eq!(entry["annotations"]["openWorldHint"], false);
        assert!(entry.get("input_schema").is_none(), "{entry}");
        assert!(listed.get("nextCursor").is_none());
    }

    #[test]
    fn the_annotation_presets_say_what_they_claim() {
        // Through the wire shape, which is what a host reads.
        let wire = |a: ToolAnnotations| serde_json::to_value(a).unwrap();
        let open_world = wire(ToolAnnotations::OPEN_WORLD);
        assert_eq!(open_world["openWorldHint"], true);
        assert_eq!(open_world["readOnlyHint"], false);
        let destructive = wire(ToolAnnotations::DESTRUCTIVE);
        assert_eq!(destructive["destructiveHint"], true);
        assert_eq!(destructive["idempotentHint"], true);
        let mutating = wire(ToolAnnotations::MUTATING);
        assert_eq!(mutating["readOnlyHint"], false);
        assert_eq!(mutating["destructiveHint"], false);
        assert_eq!(wire(ToolAnnotations::default()), mutating);
    }

    #[test]
    fn a_text_result_carries_one_text_block_and_optional_structured_content() {
        let plain = text_result("done", false, None);
        assert_eq!(plain["content"][0]["type"], "text");
        assert_eq!(plain["content"][0]["text"], "done");
        assert_eq!(plain["isError"], false);
        assert!(plain.get("structuredContent").is_none());

        let failed = text_result(String::from("boom"), true, Some(json!({ "run_id": "r1" })));
        assert_eq!(failed["isError"], true);
        assert_eq!(failed["structuredContent"]["run_id"], "r1");
    }

    #[test]
    fn ping_is_an_empty_object() {
        assert_eq!(ping_result(), json!({}));
    }

    #[test]
    fn an_unknown_method_is_refused_with_the_standard_code_and_the_shared_wording() {
        let (code, message) = method_not_found("server/discover");
        assert_eq!(code, -32601);
        assert_eq!(code, METHOD_NOT_FOUND);
        assert_eq!(message, "Leviath does not implement 'server/discover'");
    }

    #[test]
    fn progress_params_carry_the_client_token_verbatim() {
        let params = progress_params(&json!("tok-1"), 3, "run r1 started");
        assert_eq!(params["progressToken"], "tok-1");
        assert_eq!(params["progress"], 3);
        assert_eq!(params["message"], "run r1 started");
        assert!(params.get("total").is_none());
        let numeric = progress_params(&json!(7), 1, "");
        assert_eq!(numeric["progressToken"], 7);
    }
}
