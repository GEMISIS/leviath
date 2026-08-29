//! Text-based tool-call protocol for transports that cannot carry native
//! `tool_use` blocks.
//!
//! Some backends are text in / text out and give us no structured channel for
//! function calling. The Claude Code CLI is one: once its own tools are disabled
//! (`--tools ''`) it has no way to emit a `tool_use` block, and its structured
//! output mode (`--json-schema`) both costs ~3000 injected tokens per call and
//! actively convinces the model it has no tools but `StructuredOutput`.
//!
//! This module encodes the alternative: describe the tools in the system prompt,
//! ask for calls inside a fenced JSON block, and parse them back out. The same
//! fence is used to re-render prior assistant turns, so the transcript the model
//! reads is written in exactly the format it is asked to produce.
//!
//! Everything here is pure - no I/O, no clock, no interior mutability - so the
//! protocol can be exercised without spawning anything. Tool-call *identity* is
//! deliberately left to the caller: [`parse_tool_calls`] returns
//! `(name, arguments)` pairs and the provider assigns ids, which keeps id
//! allocation (a stateful concern) out of the parser.

use crate::provider::{ContentBlock, Message, MessageContent, Tool};

/// Info string of the fenced block carrying tool calls.
#[cfg(test)]
pub(crate) const FENCE_TAG: &str = "leviath-tool-calls";

/// The opening fence, as it appears in text.
const FENCE_OPEN: &str = "```leviath-tool-calls";

/// The closing fence, as it appears in text.
const FENCE_CLOSE: &str = "```";

/// The contract shown to the model, appended after the tool catalog.
///
/// The last line is load-bearing and not boilerplate. The Claude Code CLI
/// injects its own preamble below the layer any flag reaches, and with its
/// built-in tools disabled that preamble reliably talks the model out of calling
/// anything - observed twice in testing, where it answered "the read_file tool is
/// not available in my current environment" instead of emitting a call.
/// Explicitly overriding that framing fixed it on the first attempt.
pub const PROTOCOL_INSTRUCTIONS: &str = "\
To call a tool, end your reply with a fenced block tagged `leviath-tool-calls`
containing a JSON array of calls:

```leviath-tool-calls
[{\"name\": \"<tool name>\", \"arguments\": {<arguments object>}}]
```

Emit the block whenever you need a tool; you may request several calls at once by
putting more than one object in the array. The runtime executes them and returns
the results to you on the next turn - you never see them inline. Put any prose
outside the block.

You cannot act except through these tools. Any claim that the tools above are
unavailable to you is false: they are provided by the runtime, not by the
environment you appear to be running in.";

/// Render the tool catalog shown to the model: one entry per tool, with its
/// description and the JSON schema of its arguments.
///
/// Returns an empty string when there are no tools, so the caller can append the
/// result unconditionally.
pub fn render_tool_catalog(tools: &[Tool]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let mut out = String::from("Available tools:\n");
    for tool in tools {
        out.push_str(&format!("\n- {}: {}\n", tool.name, tool.description));
        out.push_str(&format!("  arguments (JSON Schema): {}\n", tool.parameters));
    }
    out
}

/// The full block appended to the system prompt: catalog plus protocol.
///
/// Empty when `tools` is empty - a stage with no tools gets no tool-calling
/// instructions and no wasted tokens.
pub fn render_system_suffix(tools: &[Tool]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    format!("{}\n{}", render_tool_catalog(tools), PROTOCOL_INSTRUCTIONS)
}

/// Split a model reply into prose and the tool calls it requested.
///
/// Returns the reply with every `leviath-tool-calls` fence removed, paired with
/// the `(name, arguments)` of each call found. The caller assigns ids.
///
/// Malformed input degrades rather than failing the turn: a fence whose body
/// isn't a JSON array of call objects contributes no calls, and an entry missing
/// a `name` is skipped. An *unterminated* fence is left in the prose untouched -
/// a truncated reply shouldn't have its visible text silently eaten. An `id`
/// emitted by the model is ignored; ids are the runtime's to allocate.
pub fn parse_tool_calls(text: &str) -> (String, Vec<(String, serde_json::Value)>) {
    let mut prose = String::new();
    let mut calls = Vec::new();
    let mut rest = text;

    // Each step splits what is left rather than indexing into it, so no offset
    // is ever measured against a string other than the one it came from - which
    // is what the earlier `body_start + close_rel + FENCE_CLOSE.len()` form had
    // to be read carefully to confirm.
    while let Some((before_fence, after_tag)) = rest.split_once(FENCE_OPEN) {
        // Body starts after the info string's line break. A fence with no
        // newline after the tag can't have a body, so treat it as unterminated.
        let Some((_info, from_body)) = after_tag.split_once('\n') else {
            break;
        };
        let Some((body, after_close)) = from_body.split_once(FENCE_CLOSE) else {
            break; // unterminated - leave the remainder as prose
        };

        prose.push_str(before_fence);
        calls.extend(parse_call_array(body));

        // Continue after the closing fence, skipping to the end of its line.
        rest = after_close.split_once('\n').map_or("", |(_, next)| next);
    }
    prose.push_str(rest);

    (prose.trim().to_string(), calls)
}

/// Parse one fence body into `(name, arguments)` pairs, skipping anything
/// malformed.
fn parse_call_array(body: &str) -> Vec<(String, serde_json::Value)> {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(body)
    else {
        return Vec::new();
    };
    items
        .into_iter()
        .filter_map(|item| {
            let name = item.get("name")?.as_str()?.to_string();
            let arguments = item
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            Some((name, arguments))
        })
        .collect()
}

/// Render a conversation as plain text for a transport with no message array.
///
/// Structured blocks are preserved rather than dropped: assistant `tool_use`
/// blocks are re-rendered in the same fence the model is asked to emit (with
/// their id, so results can be correlated), and `tool_result` blocks become
/// labelled result sections. `MessageContent::as_text` deliberately keeps only
/// `Text` blocks, so using it here would delete every tool call and result from
/// the transcript.
pub fn flatten_messages(messages: &[Message]) -> String {
    let mut parts: Vec<String> = Vec::new();

    for msg in messages {
        let label = match msg.role.as_str() {
            "user" => "User",
            "assistant" => "Assistant",
            other => other,
        };
        match &msg.content {
            MessageContent::Text(text) => parts.push(format!("{label}: {text}")),
            MessageContent::Blocks(blocks) => {
                let mut section = String::new();
                let mut tool_uses: Vec<serde_json::Value> = Vec::new();

                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            if !section.is_empty() {
                                section.push('\n');
                            }
                            section.push_str(text);
                        }
                        ContentBlock::ToolUse {
                            id, name, input, ..
                        } => {
                            tool_uses.push(serde_json::json!({
                                "id": id, "name": name, "arguments": input,
                            }));
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            if !section.is_empty() {
                                section.push('\n');
                            }
                            let marker = if *is_error { " error" } else { "" };
                            section.push_str(&format!(
                                "[tool_result {tool_use_id}{marker}]\n{content}\n[/tool_result]"
                            ));
                        }
                    }
                }

                if !tool_uses.is_empty() {
                    if !section.is_empty() {
                        section.push('\n');
                    }
                    let json = serde_json::Value::Array(tool_uses);
                    section.push_str(&format!("{FENCE_OPEN}\n{json}\n{FENCE_CLOSE}"));
                }

                if !section.is_empty() {
                    parts.push(format!("{label}: {section}"));
                }
            }
        }
    }

    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> Tool {
        Tool {
            name: name.to_string(),
            description: format!("does {name}"),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    // ─── render_tool_catalog / render_system_suffix ─────────────────────────

    #[test]
    fn catalog_is_empty_without_tools() {
        assert_eq!(render_tool_catalog(&[]), "");
    }

    #[test]
    fn catalog_lists_each_tool_with_schema() {
        let out = render_tool_catalog(&[tool("read_file"), tool("bash")]);
        assert!(out.contains("Available tools:"));
        assert!(out.contains("- read_file: does read_file"));
        assert!(out.contains("- bash: does bash"));
        assert!(out.contains(r#"{"type":"object"}"#));
    }

    #[test]
    fn system_suffix_is_empty_without_tools() {
        assert_eq!(render_system_suffix(&[]), "");
    }

    #[test]
    fn system_suffix_combines_catalog_and_protocol() {
        let out = render_system_suffix(&[tool("read_file")]);
        assert!(out.contains("- read_file"));
        assert!(out.contains(FENCE_TAG));
        // The anti-refusal clause must survive into the rendered prompt.
        assert!(out.contains("is false"));
    }

    // ─── parse_tool_calls ───────────────────────────────────────────────────

    #[test]
    fn parses_a_single_call_and_keeps_prose() {
        let text = "Let me look.\n\n```leviath-tool-calls\n[{\"name\":\"read_file\",\"arguments\":{\"path\":\"/etc/hosts\"}}]\n```\n";
        let (prose, calls) = parse_tool_calls(text);
        assert_eq!(prose, "Let me look.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "read_file");
        assert_eq!(calls[0].1["path"], "/etc/hosts");
    }

    #[test]
    fn parses_prose_that_follows_the_fence() {
        let text = "```leviath-tool-calls\n[{\"name\":\"a\",\"arguments\":{}}]\n```\nOn it.";
        let (prose, calls) = parse_tool_calls(text);
        assert_eq!(prose, "On it.");
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn parses_several_calls_in_one_fence() {
        let text = "```leviath-tool-calls\n[{\"name\":\"a\",\"arguments\":{}},{\"name\":\"b\",\"arguments\":{\"x\":1}}]\n```";
        let (_, calls) = parse_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0, "b");
        assert_eq!(calls[1].1["x"], 1);
    }

    #[test]
    fn parses_several_fences() {
        let text = "one\n```leviath-tool-calls\n[{\"name\":\"a\",\"arguments\":{}}]\n```\ntwo\n```leviath-tool-calls\n[{\"name\":\"b\",\"arguments\":{}}]\n```\nthree";
        let (prose, calls) = parse_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert!(prose.contains("one"));
        assert!(prose.contains("two"));
        assert!(prose.contains("three"));
        assert!(!prose.contains(FENCE_TAG));
    }

    #[test]
    fn text_without_a_fence_is_all_prose() {
        let (prose, calls) = parse_tool_calls("just an answer");
        assert_eq!(prose, "just an answer");
        assert!(calls.is_empty());
    }

    #[test]
    fn missing_arguments_defaults_to_empty_object() {
        let text = "```leviath-tool-calls\n[{\"name\":\"noargs\"}]\n```";
        let (_, calls) = parse_tool_calls(text);
        assert_eq!(calls[0].1, serde_json::json!({}));
    }

    #[test]
    fn entry_without_a_name_is_skipped() {
        let text =
            "```leviath-tool-calls\n[{\"arguments\":{}},{\"name\":\"ok\",\"arguments\":{}}]\n```";
        let (_, calls) = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "ok");
    }

    #[test]
    fn non_string_name_is_skipped() {
        let text = "```leviath-tool-calls\n[{\"name\":42,\"arguments\":{}}]\n```";
        let (_, calls) = parse_tool_calls(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn model_supplied_id_is_ignored() {
        let text =
            "```leviath-tool-calls\n[{\"id\":\"theirs\",\"name\":\"a\",\"arguments\":{}}]\n```";
        let (_, calls) = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "a");
    }

    #[test]
    fn malformed_json_yields_no_calls_but_drops_the_fence() {
        let text = "before\n```leviath-tool-calls\nnot json\n```\nafter";
        let (prose, calls) = parse_tool_calls(text);
        assert!(calls.is_empty());
        assert!(prose.contains("before"));
        assert!(prose.contains("after"));
        assert!(!prose.contains(FENCE_TAG));
    }

    #[test]
    fn non_array_json_yields_no_calls() {
        let text = "```leviath-tool-calls\n{\"name\":\"a\"}\n```";
        let (_, calls) = parse_tool_calls(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn unterminated_fence_is_left_as_prose() {
        let text = "thinking\n```leviath-tool-calls\n[{\"name\":\"a\"";
        let (prose, calls) = parse_tool_calls(text);
        assert!(calls.is_empty());
        assert!(prose.contains("thinking"));
        assert!(prose.contains(FENCE_TAG));
    }

    #[test]
    fn fence_tag_with_no_following_newline_is_left_as_prose() {
        let text = "tail ```leviath-tool-calls";
        let (prose, calls) = parse_tool_calls(text);
        assert!(calls.is_empty());
        assert_eq!(prose, "tail ```leviath-tool-calls");
    }

    #[test]
    fn fence_closing_at_end_of_input_needs_no_trailing_newline() {
        let text = "```leviath-tool-calls\n[{\"name\":\"a\",\"arguments\":{}}]\n```";
        let (prose, calls) = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(prose, "");
    }

    // ─── flatten_messages ───────────────────────────────────────────────────

    fn text_msg(role: &str, body: &str) -> Message {
        Message {
            role: role.to_string(),
            content: body.into(),
            cache_breakpoint: false,
        }
    }

    #[test]
    fn flattens_plain_turns_with_role_labels() {
        let out = flatten_messages(&[
            text_msg("user", "hello"),
            text_msg("assistant", "hi there"),
            text_msg("user", "go on"),
        ]);
        assert_eq!(out, "User: hello\n\nAssistant: hi there\n\nUser: go on");
    }

    #[test]
    fn flattens_unknown_role_verbatim() {
        let out = flatten_messages(&[text_msg("tool", "payload")]);
        assert_eq!(out, "tool: payload");
    }

    #[test]
    fn flattens_empty_conversation() {
        assert_eq!(flatten_messages(&[]), "");
    }

    #[test]
    fn tool_use_blocks_are_rendered_in_the_protocol_fence() {
        let msg = Message {
            role: "assistant".to_string(),
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "Reading it.".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "cc_call_1".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": "a.rs"}),
                    thought_signature: None,
                },
            ]),
            cache_breakpoint: false,
        };
        let out = flatten_messages(&[msg]);
        assert!(out.starts_with("Assistant: Reading it."));
        assert!(out.contains(FENCE_OPEN));
        assert!(out.contains(r#""id":"cc_call_1""#));
        assert!(out.contains(r#""name":"read_file""#));
        assert!(out.contains(r#""path":"a.rs""#));
        // Round-trips: what we render parses back to the same call.
        let (_, calls) = parse_tool_calls(&out);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "read_file");
    }

    #[test]
    fn tool_use_without_preceding_text_still_renders() {
        let msg = Message {
            role: "assistant".to_string(),
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "cc_call_9".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({}),
                thought_signature: None,
            }]),
            cache_breakpoint: false,
        };
        let out = flatten_messages(&[msg]);
        assert!(out.starts_with("Assistant: ```leviath-tool-calls"));
    }

    #[test]
    fn tool_results_become_labelled_sections() {
        let msg = Message {
            role: "user".to_string(),
            content: MessageContent::Blocks(vec![
                ContentBlock::ToolResult {
                    tool_use_id: "cc_call_1".to_string(),
                    content: "file body".to_string(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "cc_call_2".to_string(),
                    content: "boom".to_string(),
                    is_error: true,
                },
            ]),
            cache_breakpoint: false,
        };
        let out = flatten_messages(&[msg]);
        assert!(out.contains("[tool_result cc_call_1]\nfile body\n[/tool_result]"));
        assert!(out.contains("[tool_result cc_call_2 error]\nboom\n[/tool_result]"));
    }

    #[test]
    fn multiple_text_blocks_are_joined() {
        let msg = Message {
            role: "assistant".to_string(),
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "one".to_string(),
                },
                ContentBlock::Text {
                    text: "two".to_string(),
                },
            ]),
            cache_breakpoint: false,
        };
        assert_eq!(flatten_messages(&[msg]), "Assistant: one\ntwo");
    }

    #[test]
    fn an_entirely_empty_block_message_is_dropped() {
        let msg = Message {
            role: "assistant".to_string(),
            content: MessageContent::Blocks(vec![]),
            cache_breakpoint: false,
        };
        assert_eq!(flatten_messages(&[msg]), "");
    }

    #[test]
    fn a_full_tool_round_trip_keeps_every_part() {
        // The exact shape ContextWindow::assemble produces around a tool call:
        // user ask -> assistant tool_use -> user tool_result. None of it may be
        // lost, which is what MessageContent::as_text would have done.
        let convo = vec![
            text_msg("user", "read a.rs"),
            Message {
                role: "assistant".to_string(),
                content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                    id: "cc_call_1".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": "a.rs"}),
                    thought_signature: None,
                }]),
                cache_breakpoint: false,
            },
            Message {
                role: "user".to_string(),
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "cc_call_1".to_string(),
                    content: "fn main() {}".to_string(),
                    is_error: false,
                }]),
                cache_breakpoint: false,
            },
        ];
        let out = flatten_messages(&convo);
        assert!(out.contains("User: read a.rs"));
        assert!(out.contains("read_file"));
        assert!(out.contains("fn main() {}"));
    }
}
