//! What actually reaches the wire.
//!
//! The load-bearing tests here are the ones that pin structured regions into
//! the body. A transport that silently drops them still answers, still passes a
//! smoke test, and quietly makes every agent worse.

use super::*;
use crate::provider::{SystemBlock, ToolCall};
use leviath_core::{CacheHint, Volatility};

fn block(region: &str, text: &str, volatility: Volatility) -> SystemBlock {
    SystemBlock {
        text: text.to_string(),
        cache_hint: CacheHint::Always,
        region: region.to_string(),
        volatility,
    }
}

fn user(text: &str) -> Message {
    Message {
        role: "user".to_string(),
        content: MessageContent::Text(text.to_string()),
        cache_breakpoint: false,
        reasoning: None,
    }
}

fn request(system: Vec<SystemBlock>, messages: Vec<Message>) -> InferenceRequest {
    InferenceRequest {
        system,
        messages,
        model: "gpt-5.6-sol".to_string(),
        max_tokens: 4096,
        temperature: 0.7,
        tools: vec![],
        extra: Value::Null,
        request_timeout_secs: None,
    }
}

fn build_default(req: &InferenceRequest) -> Value {
    build(req, "medium", "low", true)
}

/// Every `developer` item's text, in order.
fn developer_texts(body: &Value) -> Vec<String> {
    body["input"]
        .as_array()
        .expect("input is an array")
        .iter()
        .filter(|item| item["role"] == "developer")
        .map(|item| {
            item["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .to_string()
        })
        .collect()
}

#[test]
fn every_structured_region_reaches_the_body() {
    // The bug this pins is recorded in claude_code.rs: a transport that read
    // only `role == "system"` messages dropped every region, because
    // `ContextWindow::assemble` puts them in `request.system` and never
    // populates that role.
    let req = request(
        vec![
            block("task", "## task\nDo the thing.", Volatility::Stable),
            block("findings", "## findings\nOne, two.", Volatility::Grows),
            block("scratch", "## scratch\nNotes.", Volatility::Rewritten),
        ],
        vec![user("go")],
    );
    let body = build_default(&req);

    let texts = developer_texts(&body);
    assert_eq!(texts.len(), 3, "a region went missing: {texts:?}");
    assert!(texts[0].contains("Do the thing."));
    assert!(texts[1].contains("One, two."));
    assert!(texts[2].contains("Notes."));
}

#[test]
fn each_region_keeps_its_own_item_and_its_heading() {
    // One item per block, not one joined blob: the boundaries are the point.
    let req = request(
        vec![
            block("task", "## task\nA", Volatility::Stable),
            block("notes", "## notes\nB", Volatility::Stable),
        ],
        vec![user("go")],
    );
    let texts = developer_texts(&build_default(&req));
    assert_eq!(
        texts,
        vec!["## task\nA".to_string(), "## notes\nB".to_string()]
    );
}

#[test]
fn block_order_is_left_exactly_as_assembly_sorted_it() {
    // There are no cache breakpoints on this route, only implicit prefix
    // caching, so re-sorting here would throw the whole strategy away.
    let req = request(
        vec![
            block("c", "third", Volatility::Rewritten),
            block("a", "first", Volatility::Stable),
            block("b", "second", Volatility::Grows),
        ],
        vec![user("go")],
    );
    assert_eq!(
        developer_texts(&build_default(&req)),
        vec!["third", "first", "second"]
    );
}

#[test]
fn framework_hints_become_the_instructions_and_regions_do_not() {
    // A block with no region name is a hint or a tool preamble: stable
    // framework text that belongs at the head of the cached prefix.
    let req = request(
        vec![
            block("", "Batch your tool calls.", Volatility::Stable),
            block("", "The shell is zsh.", Volatility::Stable),
            block("task", "## task\nsecret region content", Volatility::Stable),
        ],
        vec![user("go")],
    );
    let body = build_default(&req);

    let instructions = body["instructions"].as_str().expect("instructions");
    assert!(instructions.contains("Batch your tool calls."));
    assert!(instructions.contains("The shell is zsh."));
    assert!(
        !instructions.contains("secret region content"),
        "a region leaked into instructions: {instructions}"
    );
    assert_eq!(developer_texts(&body).len(), 1);
}

#[test]
fn instructions_fall_back_to_a_fixed_preamble() {
    // Never varies by stage: it is the first bytes of the cached prefix.
    let req = request(
        vec![block("task", "x", Volatility::Stable)],
        vec![user("go")],
    );
    assert_eq!(build_default(&req)["instructions"], DEFAULT_INSTRUCTIONS);
}

#[test]
fn hint_blocks_join_on_a_blank_line() {
    // A single newline would run `## heading` into the previous block's body.
    let req = request(
        vec![
            block("", "## one\nA", Volatility::Stable),
            block("", "## two\nB", Volatility::Stable),
        ],
        vec![user("go")],
    );
    assert_eq!(
        build_default(&req)["instructions"],
        "## one\nA\n\n## two\nB"
    );
}

#[test]
fn a_system_role_message_becomes_a_developer_item() {
    // The compaction, edge-transform and context-transform lanes all build a
    // request with an empty `system` and a `role: "system"` message. A provider
    // that only reads `request.system` turns the instruction into user text.
    let req = request(
        vec![],
        vec![
            Message {
                role: "system".to_string(),
                content: MessageContent::Text("Summarize the following.".to_string()),
                cache_breakpoint: false,
                reasoning: None,
            },
            user("a long region to compact"),
        ],
    );
    let body = build_default(&req);

    assert_eq!(developer_texts(&body), vec!["Summarize the following."]);
    // And it is not also emitted as a message with an unusable role.
    let roles: Vec<&str> = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["role"].as_str())
        .collect();
    assert!(
        !roles.contains(&"system"),
        "a system role reached the wire: {roles:?}"
    );
}

#[test]
fn developer_items_all_precede_the_conversation() {
    let req = request(
        vec![block("task", "context", Volatility::Stable)],
        vec![user("go")],
    );
    let body = build_default(&req);
    let items = body["input"].as_array().unwrap();
    assert_eq!(items[0]["role"], "developer");
    assert_eq!(items[1]["role"], "user");
}

#[test]
fn empty_blocks_and_messages_contribute_nothing() {
    let req = request(
        vec![
            block("task", "   ", Volatility::Stable),
            block("", "\n", Volatility::Stable),
        ],
        vec![user("  ")],
    );
    let body = build_default(&req);
    assert_eq!(developer_texts(&body).len(), 0);
    assert!(body["input"].as_array().unwrap().is_empty());
    assert_eq!(body["instructions"], DEFAULT_INSTRUCTIONS);
}

#[test]
fn temperature_and_the_output_cap_are_never_sent() {
    // Both measured as `400 Unsupported parameter` on every model this route
    // serves. Sending either fails the request outright.
    let req = request(vec![], vec![user("go")]);
    let body = build_default(&req);
    assert!(body.get("temperature").is_none(), "{body}");
    assert!(body.get("max_output_tokens").is_none(), "{body}");
    assert!(body.get("max_tokens").is_none(), "{body}");
    assert!(body.get("prompt_cache_retention").is_none(), "{body}");
}

#[test]
fn the_stateless_flags_are_always_set() {
    let body = build_default(&request(vec![], vec![user("go")]));
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
}

#[test]
fn a_tool_call_round_trips_with_the_call_id_and_string_arguments() {
    let req = request(
        vec![],
        vec![
            user("read it"),
            Message {
                role: "assistant".to_string(),
                content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                    id: "call_abc".to_string(),
                    name: "read_file".to_string(),
                    input: json!({ "path": "/etc/hostname" }),
                    thought_signature: None,
                }]),
                cache_breakpoint: false,
                reasoning: None,
            },
            Message {
                role: "user".to_string(),
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "call_abc".to_string(),
                    content: "probe-host".to_string(),
                    is_error: false,
                }]),
                cache_breakpoint: false,
                reasoning: None,
            },
        ],
    );
    let items = build_default(&req)["input"].as_array().unwrap().clone();

    let call = items
        .iter()
        .find(|i| i["type"] == "function_call")
        .expect("call");
    assert_eq!(call["call_id"], "call_abc");
    assert_eq!(call["name"], "read_file");
    // A JSON string, not an object: the backend rejects an object here.
    assert_eq!(call["arguments"], json!(r#"{"path":"/etc/hostname"}"#));

    let out = items
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .expect("output");
    assert_eq!(out["call_id"], "call_abc");
    assert_eq!(out["output"], "probe-host");
}

#[test]
fn a_failed_tool_result_keeps_saying_so() {
    // `function_call_output` has no error flag; dropping the distinction would
    // present a failure to the model as a successful result.
    let req = request(
        vec![],
        vec![Message {
            role: "user".to_string(),
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "call_x".to_string(),
                content: "permission denied".to_string(),
                is_error: true,
            }]),
            cache_breakpoint: false,
            reasoning: None,
        }],
    );
    let items = build_default(&req)["input"].as_array().unwrap().clone();
    let out = items
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .unwrap();
    assert_eq!(out["output"], "[error] permission denied");
}

#[test]
fn assistant_text_beside_a_tool_call_survives_as_its_own_item() {
    let req = request(
        vec![],
        vec![Message {
            role: "assistant".to_string(),
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "Let me look.".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    input: json!({}),
                    thought_signature: None,
                },
            ]),
            cache_breakpoint: false,
            reasoning: None,
        }],
    );
    let items = build_default(&req)["input"].as_array().unwrap().clone();
    assert_eq!(items[0]["content"][0]["text"], "Let me look.");
    // Assistant text is `output_text`, not `input_text`.
    assert_eq!(items[0]["content"][0]["type"], "output_text");
    assert_eq!(items[1]["type"], "function_call");
}

#[test]
fn trailing_assistant_text_after_a_tool_call_is_not_lost() {
    let req = request(
        vec![],
        vec![Message {
            role: "assistant".to_string(),
            content: MessageContent::Blocks(vec![
                ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "t".to_string(),
                    input: json!({}),
                    thought_signature: None,
                },
                ContentBlock::Text {
                    text: "and then this".to_string(),
                },
            ]),
            cache_breakpoint: false,
            reasoning: None,
        }],
    );
    let items = build_default(&req)["input"].as_array().unwrap().clone();
    assert_eq!(items.last().unwrap()["content"][0]["text"], "and then this");
}

#[test]
fn a_gemini_thought_signature_never_reaches_this_wire() {
    // One provider's opaque token in shared history is replayed to whichever
    // provider runs next; an unknown key is a hard rejection.
    let req = request(
        vec![],
        vec![Message {
            role: "assistant".to_string(),
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "t".to_string(),
                input: json!({}),
                thought_signature: Some("gemini-token".to_string()),
            }]),
            cache_breakpoint: false,
            reasoning: None,
        }],
    );
    let rendered = build_default(&req).to_string();
    assert!(!rendered.contains("gemini-token"), "leaked: {rendered}");
    assert!(!rendered.contains("thought_signature"));
}

#[test]
fn a_replayed_reasoning_item_precedes_the_turn_it_belongs_to() {
    let req = request(
        vec![],
        vec![Message {
            role: "assistant".to_string(),
            content: MessageContent::Text("42".to_string()),
            cache_breakpoint: false,
            reasoning: Some("sealed-blob".to_string()),
        }],
    );
    let items = build_default(&req)["input"].as_array().unwrap().clone();
    assert_eq!(items[0]["type"], "reasoning");
    assert_eq!(items[0]["encrypted_content"], "sealed-blob");
    assert_eq!(items[1]["content"][0]["text"], "42");
    assert_eq!(
        build_default(&req)["include"],
        json!(["reasoning.encrypted_content"])
    );
}

#[test]
fn reasoning_replay_can_be_turned_off_entirely() {
    // Asking for a blob that will be dropped is response bytes for nothing.
    let req = request(
        vec![],
        vec![Message {
            role: "assistant".to_string(),
            content: MessageContent::Text("42".to_string()),
            cache_breakpoint: false,
            reasoning: Some("sealed-blob".to_string()),
        }],
    );
    let body = build(&req, "medium", "low", false);
    assert!(body.get("include").is_none(), "{body}");
    assert!(!body.to_string().contains("sealed-blob"));
}

#[test]
fn a_user_turn_never_carries_a_reasoning_item() {
    // Only an assistant turn owns one; replaying it against a user message
    // would be a shape the backend has never issued.
    let req = request(
        vec![],
        vec![Message {
            role: "user".to_string(),
            content: MessageContent::Text("go".to_string()),
            cache_breakpoint: false,
            reasoning: Some("not-mine".to_string()),
        }],
    );
    assert!(!build_default(&req).to_string().contains("not-mine"));
}

#[test]
fn effort_none_sends_no_reasoning_block_at_all() {
    let req = request(vec![], vec![user("go")]);
    let body = build(&req, "none", "low", true);
    assert!(body.get("reasoning").is_none(), "{body}");
    assert!(body.get("include").is_none(), "{body}");
    assert_eq!(body["text"]["verbosity"], "low");
}

#[test]
fn effort_and_verbosity_reach_the_body() {
    let req = request(vec![], vec![user("go")]);
    let body = build(&req, "xhigh", "high", true);
    assert_eq!(body["reasoning"]["effort"], "xhigh");
    assert_eq!(body["reasoning"]["summary"], "auto");
    assert_eq!(body["text"]["verbosity"], "high");
}

#[test]
fn tools_use_the_flat_responses_shape() {
    let mut req = request(vec![], vec![user("go")]);
    req.tools = vec![Tool {
        name: "read_file".to_string(),
        description: "Read a file.".to_string(),
        parameters: json!({ "type": "object", "properties": {} }),
    }];
    let body = build_default(&req);
    let tool = &body["tools"][0];
    assert_eq!(tool["type"], "function");
    assert_eq!(tool["name"], "read_file");
    // Not the Chat Completions `function: { ... }` wrapper.
    assert!(tool.get("function").is_none(), "{tool}");
    assert_eq!(tool["strict"], false);
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["parallel_tool_calls"], true);
}

#[test]
fn a_request_with_no_tools_sends_no_tool_keys() {
    let body = build_default(&request(vec![], vec![user("go")]));
    assert!(body.get("tools").is_none(), "{body}");
    assert!(body.get("tool_choice").is_none(), "{body}");
}

#[test]
fn the_cache_key_holds_still_across_a_stage_and_fits_the_limit() {
    // Stable across turns is the whole point: folding in the sliding window
    // would mint a new key every iteration and defeat the mechanism.
    let stable = block("task", "the task", Volatility::Stable);
    let first = request(vec![stable.clone()], vec![user("turn one")]);
    let second = request(
        vec![
            stable.clone(),
            block("scratch", "grew since", Volatility::Rewritten),
        ],
        vec![user("turn one"), user("turn two")],
    );
    let key = build_default(&first)["prompt_cache_key"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(build_default(&second)["prompt_cache_key"], key.as_str());
    assert!(key.len() <= 64, "key is {} chars: {key}", key.len());
    assert!(key.starts_with("lev-"));
}

#[test]
fn the_cache_key_moves_when_the_stable_prefix_does() {
    let a = request(vec![block("task", "one", Volatility::Stable)], vec![]);
    let b = request(vec![block("task", "two", Volatility::Stable)], vec![]);
    assert_ne!(
        build_default(&a)["prompt_cache_key"],
        build_default(&b)["prompt_cache_key"]
    );
}

#[test]
fn the_cache_key_moves_when_the_model_does() {
    // A different model is a different cache, so sharing a key would ask the
    // backend to reuse a prefix it never stored.
    let mut a = request(vec![block("task", "one", Volatility::Stable)], vec![]);
    let mut b = a.clone();
    a.model = "gpt-5.6-sol".to_string();
    b.model = "gpt-5.6-luna".to_string();
    assert_ne!(
        build_default(&a)["prompt_cache_key"],
        build_default(&b)["prompt_cache_key"]
    );
}

#[test]
fn a_long_region_name_cannot_overflow_the_cache_key() {
    // Fixed width by construction; this pins that the construction is what it
    // claims rather than a length that happens to fit today.
    let req = request(
        vec![block(
            &"r".repeat(4096),
            &"x".repeat(50_000),
            Volatility::Stable,
        )],
        vec![],
    );
    let key = build_default(&req)["prompt_cache_key"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(key.len(), 20, "got {key}");
}

#[test]
fn a_tool_call_from_a_response_maps_by_call_id() {
    // Guards the id/call_id confusion at the other end of the round trip: the
    // id stored on a ToolCall is what a later function_call_output echoes.
    let call = ToolCall {
        id: "call_from_stream".to_string(),
        name: "t".to_string(),
        arguments: json!({}),
        thought_signature: None,
    };
    let req = request(
        vec![],
        vec![Message {
            role: "assistant".to_string(),
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: call.id.clone(),
                name: call.name.clone(),
                input: call.arguments.clone(),
                thought_signature: None,
            }]),
            cache_breakpoint: false,
            reasoning: None,
        }],
    );
    let items = build_default(&req)["input"].as_array().unwrap().clone();
    assert_eq!(items[0]["call_id"], "call_from_stream");
    assert!(
        items[0].get("id").is_none(),
        "the item id must not be echoed"
    );
}

#[test]
fn stage_parameters_reach_the_body() {
    // `[stages.<n>.model.parameters]` beyond the two the request struct names,
    // and the titling lane's reasoning override, both travel here.
    let mut req = request(vec![], vec![user("go")]);
    req.extra = json!({
        "top_p": 0.1,
        "reasoning": { "effort": "minimal" },
        "text": { "verbosity": "low" },
    });
    let body = build_default(&req);
    assert_eq!(body["top_p"], json!(0.1));
    assert_eq!(body["reasoning"]["effort"], "minimal");
    assert_eq!(body["text"]["verbosity"], "low");
}

#[test]
fn a_stage_cannot_smuggle_a_rejected_parameter_through_its_parameters() {
    // Both are `400 Unsupported parameter` on this route, so a stage that set
    // one would fail every request rather than have it quietly ignored.
    let mut req = request(vec![], vec![user("go")]);
    req.extra = json!({
        "temperature": 0.7,
        "max_output_tokens": 1000,
        "max_tokens": 1000,
        "prompt_cache_retention": "24h",
    });
    let body = build_default(&req);
    assert!(body.get("temperature").is_none(), "{body}");
    assert!(body.get("max_output_tokens").is_none(), "{body}");
    assert!(body.get("max_tokens").is_none(), "{body}");
    assert!(body.get("prompt_cache_retention").is_none(), "{body}");
}

#[test]
fn a_null_extra_changes_nothing() {
    let req = request(vec![], vec![user("go")]);
    assert_eq!(build_default(&req)["model"], "gpt-5.6-sol");
}
