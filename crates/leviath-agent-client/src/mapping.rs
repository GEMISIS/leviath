//! Pure translations between Leviath's own types and the Agent Client Protocol.
//!
//! Everything here is a total function over plain data - no I/O, no daemon, no
//! async - so the stdio server in `leviath-cli` is left with only sequencing to
//! do, and every mapping decision is unit-testable in isolation.

use leviath_core::interaction::{InteractionKind, InteractionRequest};
use leviath_core::run_meta::RunStatus;

use crate::protocol::{
    ContentBlock, PermissionOption, PermissionOptionKind, RequestPermissionParams, StopReason,
    ToolCallRef, ToolCallStatus, ToolKind,
};

/// The option id returned when the user approves a single tool call.
pub const OPTION_ALLOW_ONCE: &str = "allow-once";
/// The option id returned when the user approves this tool for the whole session.
pub const OPTION_ALLOW_ALWAYS: &str = "allow-always";
/// The option id returned when the user rejects a tool call.
pub const OPTION_REJECT_ONCE: &str = "reject-once";

/// Flatten a prompt's content blocks into the single task/message string Leviath
/// agents consume.
///
/// `text` blocks contribute their text. `resource` blocks (the `embeddedContext`
/// capability) contribute their inlined text under a `--- <uri> ---` header, so
/// the model can tell attached context from the instruction itself. Every other
/// block kind - `image`, `audio`, `resource_link` - is dropped: we advertise no
/// support for them, and silently ignoring one block is far better than failing
/// the whole prompt.
///
/// Blocks are joined with a blank line and the result is trimmed, so a prompt of
/// only unsupported blocks yields `""` (which the caller treats as an error
/// rather than spawning an agent with an empty task).
pub fn flatten_prompt(blocks: &[ContentBlock]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        match block.kind.as_str() {
            "text" => {
                if let Some(text) = block
                    .text
                    .as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                {
                    parts.push(text.to_string());
                }
            }
            "resource" => {
                if let Some(resource) = &block.resource
                    && let Some(text) = resource.text.as_deref()
                {
                    parts.push(format!("--- {} ---\n{}", resource.uri, text));
                }
            }
            _ => {}
        }
    }
    parts.join("\n\n").trim().to_string()
}

/// Parse `---region:<name>---` markers out of a flattened prompt into a
/// name→content map.
///
/// A line that is exactly `---region:<name>---` (after trimming) opens a region
/// block; its content runs until the next `---region:...---` marker, an
/// `---end-regions---` line, or the end of the text. Any text before the first
/// marker becomes the `task` region. With **no** markers at all, the whole text
/// is returned as `{ "task": text }` - the exact pre-feature behavior, so hosts
/// that don't use markers are unaffected.
///
/// Region bodies are trimmed; empty blocks are dropped. Pure - no I/O.
pub fn parse_region_markers(text: &str) -> std::collections::HashMap<String, String> {
    use std::collections::HashMap;

    let marker_name = |line: &str| -> Option<String> {
        let t = line.trim();
        t.strip_prefix("---region:")
            .and_then(|rest| rest.strip_suffix("---"))
            .map(|n| n.trim().to_string())
    };

    let mut out = HashMap::new();
    // Current region name (None = the leading "task" block) and its accumulated
    // lines. `ended` becomes true after `---end-regions---`.
    let mut current: Option<String> = None;
    let mut buf: Vec<&str> = Vec::new();
    let mut ended = false;
    let mut saw_marker = false;

    let flush = |name: &Option<String>, buf: &mut Vec<&str>, out: &mut HashMap<String, String>| {
        let body = buf.join("\n");
        let body = body.trim();
        if !body.is_empty() {
            let key = name.clone().unwrap_or_else(|| "task".to_string());
            out.insert(key, body.to_string());
        }
        buf.clear();
    };

    for line in text.lines() {
        if ended {
            break;
        }
        if line.trim() == "---end-regions---" {
            flush(&current, &mut buf, &mut out);
            ended = true;
            continue;
        }
        if let Some(name) = marker_name(line) {
            flush(&current, &mut buf, &mut out);
            current = Some(name);
            saw_marker = true;
            continue;
        }
        buf.push(line);
    }
    if !ended {
        flush(&current, &mut buf, &mut out);
    }

    if !saw_marker {
        // No markers: preserve exact legacy behavior (whole text → task).
        let mut out = HashMap::new();
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            out.insert("task".to_string(), trimmed.to_string());
        }
        return out;
    }
    out
}

/// The stop reason to report for a run that has reached `status`, or `None`
/// while the run has not stopped at all.
///
/// `Error` maps to `refusal` rather than inventing a failure code: the
/// protocol has no "the agent broke" reason, and `refusal` is the only one
/// that tells the host the turn produced no usable answer. Non-terminal
/// statuses yield `None` so a poller can distinguish "still running" from
/// "ended" without a second status check.
pub fn stop_reason_for(status: &RunStatus) -> Option<StopReason> {
    match status {
        RunStatus::Complete | RunStatus::CompleteInteractive => Some(StopReason::EndTurn),
        RunStatus::Error => Some(StopReason::Refusal),
        RunStatus::Cancelled => Some(StopReason::Cancelled),
        RunStatus::Starting | RunStatus::Running | RunStatus::WaitingInput => None,
    }
}

/// [`stop_reason_for`] over the string labels carried by completion events,
/// which report a terminal status by name rather than as a [`RunStatus`].
/// An unexpected label reads as an ordinary end of turn.
pub fn stop_reason_for_label(status: &str) -> StopReason {
    match status {
        "cancelled" => StopReason::Cancelled,
        "error" => StopReason::Refusal,
        _ => StopReason::EndTurn,
    }
}

/// Build a `session/request_permission` request from a Leviath tool-approval
/// interaction.
///
/// The offered options mirror what Leviath's own approval prompt supports:
/// approve once, approve for the rest of the session
/// ([`leviath_core::interaction::ApprovalScope::Session`]), or reject. There is
/// deliberately no "reject always" - Leviath has no persistent per-tool denylist
/// to record it in, and offering a choice we cannot honour would be a lie.
pub fn permission_request(
    session_id: &str,
    request: &InteractionRequest,
) -> RequestPermissionParams {
    RequestPermissionParams {
        session_id: session_id.to_string(),
        tool_call: ToolCallRef {
            tool_call_id: request.id.clone(),
            title: permission_title(request),
            kind: tool_kind_for(request.tool_name.as_deref()),
            status: ToolCallStatus::Pending,
        },
        options: vec![
            PermissionOption {
                option_id: OPTION_ALLOW_ONCE.to_string(),
                name: "Allow once".to_string(),
                kind: PermissionOptionKind::AllowOnce,
            },
            PermissionOption {
                option_id: OPTION_ALLOW_ALWAYS.to_string(),
                name: "Allow for this session".to_string(),
                kind: PermissionOptionKind::AllowAlways,
            },
            PermissionOption {
                option_id: OPTION_REJECT_ONCE.to_string(),
                name: "Reject".to_string(),
                kind: PermissionOptionKind::RejectOnce,
            },
        ],
    }
}

/// A one-line summary of the tool call awaiting approval: the tool name when the
/// request carries one, else the prompt Leviath would have shown a human.
fn permission_title(request: &InteractionRequest) -> String {
    match request.tool_name.as_deref() {
        Some(name) => name.to_string(),
        None => request.prompt.clone(),
    }
}

/// Classify a Leviath tool name into the protocol's tool-kind taxonomy, so hosts
/// can pick an icon and phrase the approval prompt.
///
/// Unrecognised names - including every MCP tool, whose names are arbitrary -
/// fall back to [`ToolKind::Other`].
fn tool_kind_for(tool_name: Option<&str>) -> ToolKind {
    match tool_name {
        Some("read_file" | "read_files" | "list_files" | "grep") => ToolKind::Read,
        Some("write_file" | "edit_file" | "apply_patch") => ToolKind::Edit,
        Some("delete_file") => ToolKind::Delete,
        Some("move_file") => ToolKind::Move,
        Some("search" | "web_search") => ToolKind::Search,
        Some("bash" | "run_command" | "shell") => ToolKind::Execute,
        Some("fetch" | "web_fetch" | "http_get") => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

/// Whether an interaction can be answered over the protocol at all.
///
/// Only [`InteractionKind::ToolApproval`] maps onto
/// `session/request_permission`. Free-text questions, multiple choice, confirms
/// and in-place document edits have no protocol equivalent, so the server
/// surfaces those as agent output and lets the next `session/prompt` carry the
/// answer.
pub fn is_permission_request(request: &InteractionRequest) -> bool {
    matches!(request.kind, InteractionKind::ToolApproval)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::EmbeddedResource;

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::text(text)
    }

    fn resource_block(uri: &str, text: Option<&str>) -> ContentBlock {
        ContentBlock {
            kind: "resource".to_string(),
            text: None,
            resource: Some(EmbeddedResource {
                uri: uri.to_string(),
                mime_type: None,
                text: text.map(str::to_string),
            }),
        }
    }

    fn approval(id: &str, tool: Option<&str>) -> InteractionRequest {
        InteractionRequest {
            id: id.to_string(),
            kind: InteractionKind::ToolApproval,
            prompt: "Run this?".to_string(),
            options: vec![],
            tool_name: tool.map(str::to_string),
            tool_arguments: None,
            required: true,
            stage_name: "implement".to_string(),
            body: None,
            body_format: Default::default(),
        }
    }

    // ─── parse_region_markers ────────────────────────────────────────────────

    #[test]
    fn markers_absent_puts_whole_text_in_task() {
        let out = parse_region_markers("just do the thing");
        assert_eq!(out.len(), 1);
        assert_eq!(
            out.get("task").map(String::as_str),
            Some("just do the thing")
        );
    }

    #[test]
    fn markers_absent_empty_text_yields_empty_map() {
        assert!(parse_region_markers("   \n  ").is_empty());
    }

    #[test]
    fn leading_text_before_first_marker_becomes_task() {
        let text = "build a parser\n---region:criteria---\nfocus on safety";
        let out = parse_region_markers(text);
        assert_eq!(out.get("task").map(String::as_str), Some("build a parser"));
        assert_eq!(
            out.get("criteria").map(String::as_str),
            Some("focus on safety")
        );
    }

    #[test]
    fn multiple_regions_and_end_marker_with_trailing_text_dropped() {
        let text = "\
---region:task---
build it
---region:criteria---
be careful
---end-regions---
this trailing text is ignored";
        let out = parse_region_markers(text);
        assert_eq!(out.get("task").map(String::as_str), Some("build it"));
        assert_eq!(out.get("criteria").map(String::as_str), Some("be careful"));
        assert_eq!(out.len(), 2, "trailing text after end marker is dropped");
    }

    #[test]
    fn empty_region_blocks_are_dropped() {
        let text = "---region:task---\nreal\n---region:empty---\n   \n";
        let out = parse_region_markers(text);
        assert_eq!(out.get("task").map(String::as_str), Some("real"));
        assert!(!out.contains_key("empty"));
    }

    // ─── flatten_prompt ──────────────────────────────────────────────────────

    #[test]
    fn flatten_joins_text_blocks_with_a_blank_line() {
        assert_eq!(
            flatten_prompt(&[text_block("first"), text_block("second")]),
            "first\n\nsecond"
        );
    }

    #[test]
    fn flatten_of_a_single_block_is_just_its_text() {
        assert_eq!(flatten_prompt(&[text_block("only")]), "only");
    }

    #[test]
    fn flatten_skips_blank_and_whitespace_only_text_blocks() {
        assert_eq!(
            flatten_prompt(&[text_block(""), text_block("  \n "), text_block("real")]),
            "real"
        );
    }

    #[test]
    fn flatten_trims_each_text_block() {
        assert_eq!(flatten_prompt(&[text_block("  padded  ")]), "padded");
    }

    #[test]
    fn flatten_skips_a_text_block_with_no_text_field() {
        let block = ContentBlock {
            kind: "text".to_string(),
            text: None,
            resource: None,
        };
        assert_eq!(flatten_prompt(&[block, text_block("kept")]), "kept");
    }

    #[test]
    fn flatten_headers_resource_blocks_with_their_uri() {
        assert_eq!(
            flatten_prompt(&[
                text_block("review this"),
                resource_block("file:///a.rs", Some("fn main() {}")),
            ]),
            "review this\n\n--- file:///a.rs ---\nfn main() {}"
        );
    }

    #[test]
    fn flatten_skips_a_resource_block_with_no_inlined_text() {
        assert_eq!(
            flatten_prompt(&[resource_block("file:///a.rs", None), text_block("kept")]),
            "kept"
        );
    }

    #[test]
    fn flatten_skips_a_resource_block_with_no_resource_field() {
        let block = ContentBlock {
            kind: "resource".to_string(),
            text: None,
            resource: None,
        };
        assert_eq!(flatten_prompt(&[block, text_block("kept")]), "kept");
    }

    #[test]
    fn flatten_drops_unsupported_block_kinds() {
        let image = ContentBlock {
            kind: "image".to_string(),
            text: Some("ignored".to_string()),
            resource: None,
        };
        assert_eq!(flatten_prompt(&[image, text_block("kept")]), "kept");
    }

    #[test]
    fn flatten_of_nothing_usable_is_empty() {
        assert_eq!(flatten_prompt(&[]), "");
        let audio = ContentBlock {
            kind: "audio".to_string(),
            text: None,
            resource: None,
        };
        assert_eq!(flatten_prompt(&[audio]), "");
    }

    // ─── stop_reason_for ─────────────────────────────────────────────────────

    #[test]
    fn stop_reason_maps_every_run_status() {
        for (status, expected) in [
            (RunStatus::Starting, None),
            (RunStatus::Running, None),
            (RunStatus::WaitingInput, None),
            (RunStatus::Complete, Some(StopReason::EndTurn)),
            (RunStatus::CompleteInteractive, Some(StopReason::EndTurn)),
            (RunStatus::Error, Some(StopReason::Refusal)),
            (RunStatus::Cancelled, Some(StopReason::Cancelled)),
        ] {
            assert_eq!(stop_reason_for(&status), expected, "status {status}");
        }
    }

    #[test]
    fn stop_reason_label_matches_the_status_mapping() {
        assert_eq!(stop_reason_for_label("cancelled"), StopReason::Cancelled);
        assert_eq!(stop_reason_for_label("error"), StopReason::Refusal);
        assert_eq!(stop_reason_for_label("complete"), StopReason::EndTurn);
        assert_eq!(stop_reason_for_label("anything-else"), StopReason::EndTurn);
    }

    // ─── permission_request ──────────────────────────────────────────────────

    #[test]
    fn permission_request_offers_once_session_and_reject() {
        let params = permission_request("s1", &approval("q1", Some("bash")));
        assert_eq!(params.session_id, "s1");
        assert_eq!(params.tool_call.tool_call_id, "q1");
        assert_eq!(params.tool_call.title, "bash");
        assert_eq!(params.tool_call.kind, ToolKind::Execute);
        assert_eq!(params.tool_call.status, ToolCallStatus::Pending);
        let ids: Vec<&str> = params
            .options
            .iter()
            .map(|o| o.option_id.as_str())
            .collect();
        assert_eq!(
            ids,
            [OPTION_ALLOW_ONCE, OPTION_ALLOW_ALWAYS, OPTION_REJECT_ONCE]
        );
        let kinds: Vec<PermissionOptionKind> = params.options.iter().map(|o| o.kind).collect();
        assert_eq!(
            kinds,
            [
                PermissionOptionKind::AllowOnce,
                PermissionOptionKind::AllowAlways,
                PermissionOptionKind::RejectOnce,
            ]
        );
    }

    #[test]
    fn permission_request_falls_back_to_the_prompt_when_there_is_no_tool_name() {
        let params = permission_request("s1", &approval("q1", None));
        assert_eq!(params.tool_call.title, "Run this?");
        assert_eq!(params.tool_call.kind, ToolKind::Other);
    }

    #[test]
    fn tool_kinds_cover_every_classification_arm() {
        for (name, expected) in [
            ("read_file", ToolKind::Read),
            ("read_files", ToolKind::Read),
            ("list_files", ToolKind::Read),
            ("grep", ToolKind::Read),
            ("write_file", ToolKind::Edit),
            ("edit_file", ToolKind::Edit),
            ("apply_patch", ToolKind::Edit),
            ("delete_file", ToolKind::Delete),
            ("move_file", ToolKind::Move),
            ("search", ToolKind::Search),
            ("web_search", ToolKind::Search),
            ("bash", ToolKind::Execute),
            ("run_command", ToolKind::Execute),
            ("shell", ToolKind::Execute),
            ("fetch", ToolKind::Fetch),
            ("web_fetch", ToolKind::Fetch),
            ("http_get", ToolKind::Fetch),
            ("mcp__whatever__thing", ToolKind::Other),
        ] {
            assert_eq!(tool_kind_for(Some(name)), expected, "tool {name}");
        }
        assert_eq!(tool_kind_for(None), ToolKind::Other);
    }

    // ─── is_permission_request ───────────────────────────────────────────────

    #[test]
    fn only_tool_approvals_are_permission_requests() {
        assert!(is_permission_request(&approval("q", Some("bash"))));
        for kind in [
            InteractionKind::FreeText,
            InteractionKind::MultipleChoice,
            InteractionKind::Confirm,
            InteractionKind::EditText,
        ] {
            let mut req = approval("q", None);
            req.kind = kind;
            assert!(!is_permission_request(&req));
        }
    }
}
