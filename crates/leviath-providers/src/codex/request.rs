//! Turning a Leviath request into a Codex Responses body.
//!
//! The Responses API takes an `input` array of typed items rather than a
//! `messages` array, and a top-level `instructions` string rather than a system
//! message. Both halves of Leviath's request have to reach it:
//!
//! - `request.system`, which is where `ContextWindow::assemble` puts every
//!   region except the sliding window.
//! - `request.messages`, which is the sliding window, and which the compaction
//!   and context-transform lanes also use to carry a `role: "system"`
//!   instruction with an empty `system` vec.
//!
//! Reading only one of those is a bug with precedent at both ends. The Claude
//! Code transport once read only `role == "system"` messages and dropped every
//! structured region on the floor; a provider that reads only `request.system`
//! would silently turn a summarisation instruction into user text.
//!
//! ## Why regions become `developer` items
//!
//! `instructions` would be the obvious home, and it is measurably free-form and
//! large enough for a small preamble. It is not large enough for the regions: a
//! single pinned region at a twenty percent budget of a 400k window is around
//! 320 KB. The `input` array has no such ceiling and, unlike the chat-template
//! providers, accepts many `developer` items - 120 were accepted where the
//! reason `openai_compat` joins its system blocks into one message is an Ollama
//! template that rejects the second. So each block keeps its own item, and the
//! region boundaries survive the way they do on OpenRouter rather than
//! collapsing into one blob the way they do on the OpenAI path.
//!
//! Block order is left exactly as assembly sorted it. There are no cache
//! breakpoints here, only implicit prefix caching, so that stable-first order
//! is not an optimisation, it is the entire caching strategy.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};

use serde_json::{Value, json};

use crate::provider::{ContentBlock, InferenceRequest, Message, MessageContent, Tool};

/// What Leviath tells the model about itself, when the request carries no
/// framework preamble of its own.
///
/// `instructions` is optional as far as the backend is concerned, but it is the
/// first bytes of the cached prefix, so it must be byte-identical on every
/// request from every stage.
const DEFAULT_INSTRUCTIONS: &str = "You are running inside Leviath, a multi-stage agent runtime. The developer \
     messages that follow carry the structured context regions for this stage. \
     Treat them as authoritative.";

/// Build the request body.
///
/// `reasoning_effort` and `verbosity` are the operator's, already validated.
/// `replay_reasoning` decides whether an assistant turn's opaque reasoning item
/// is handed back, which is the only thing that keeps a chain of thought alive
/// across turns on a backend that stores nothing.
pub fn build(
    request: &InferenceRequest,
    reasoning_effort: &str,
    verbosity: &str,
    replay_reasoning: bool,
) -> Value {
    let (instructions, region_blocks) = split_system(request);

    let mut input: Vec<Value> = Vec::new();
    for text in region_blocks {
        input.push(developer_item(&text));
    }
    for message in &request.messages {
        push_message(&mut input, message, replay_reasoning);
    }

    let mut body = json!({
        "model": request.model,
        // Mandatory. The backend rejects a stored thread on this route, which
        // is what makes reasoning replay the caller's problem.
        "store": false,
        "stream": true,
        "instructions": instructions,
        "input": input,
        "prompt_cache_key": cache_key(request),
    });

    // Deliberately absent, both measured as `400 Unsupported parameter` on
    // every model this route serves:
    //   - `temperature`, so `InferenceRequest::temperature` cannot be honoured.
    //   - `max_output_tokens`, so a stage's output cap cannot be enforced.
    // Sending either fails the whole request rather than being ignored.

    if !request.tools.is_empty() {
        body["tools"] = Value::Array(request.tools.iter().map(tool_item).collect());
        body["tool_choice"] = json!("auto");
        body["parallel_tool_calls"] = json!(true);
    }

    if reasoning_effort != "none" {
        body["reasoning"] = json!({ "effort": reasoning_effort, "summary": "auto" });
        if replay_reasoning {
            // Only worth asking for when it will be handed back; the blob is
            // response bytes that buy nothing if the next turn drops it.
            body["include"] = json!(["reasoning.encrypted_content"]);
        }
    }
    body["text"] = json!({ "verbosity": verbosity });

    body
}

/// Split the system side into the preamble and the region blocks.
///
/// `SystemBlock::region` is empty for a block that is not a region: the
/// batch-tool hint, the shell hint, a tool preamble. Those are the stable
/// framework text and belong in `instructions`, where they form the head of the
/// cached prefix. Everything with a region name is context and becomes its own
/// `developer` item.
fn split_system(request: &InferenceRequest) -> (String, Vec<String>) {
    let mut preamble: Vec<&str> = Vec::new();
    let mut regions: Vec<String> = Vec::new();

    for block in &request.system {
        let text = block.text.trim();
        if text.is_empty() {
            continue;
        }
        if block.region.is_empty() {
            preamble.push(text);
        } else {
            regions.push(text.to_string());
        }
    }

    // The compaction and context-transform lanes build a request with an empty
    // `system` and a `role: "system"` message instead. Those carry the
    // instruction for the call and have to reach the model as one.
    for message in &request.messages {
        if message.role == "system"
            && let MessageContent::Text(text) = &message.content
            && !text.trim().is_empty()
        {
            regions.push(text.trim().to_string());
        }
    }

    let instructions = match preamble.is_empty() {
        true => DEFAULT_INSTRUCTIONS.to_string(),
        // Joined on a blank line, never a single newline: a block rendered as
        // `## <region>\n<body>` loses its heading structure otherwise.
        false => preamble.join("\n\n"),
    };
    (instructions, regions)
}

/// One `developer` item. Higher priority than `user`, which is what makes it
/// the right role for context the model must not treat as a request.
fn developer_item(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "developer",
        "content": [{ "type": "input_text", "text": text }],
    })
}

/// Append a conversation message as one or more input items.
fn push_message(input: &mut Vec<Value>, message: &Message, replay_reasoning: bool) {
    // Already emitted as a developer item by `split_system`.
    if message.role == "system" {
        return;
    }

    // The reasoning item comes first: it belongs to the turn it precedes.
    if replay_reasoning
        && message.role == "assistant"
        && let Some(blob) = &message.reasoning
    {
        input.push(json!({ "type": "reasoning", "encrypted_content": blob, "summary": [] }));
    }

    match &message.content {
        MessageContent::Text(text) => {
            if !text.trim().is_empty() {
                input.push(text_item(&message.role, text));
            }
        }
        MessageContent::Blocks(blocks) => {
            let mut text = String::new();
            for block in blocks {
                match block {
                    ContentBlock::Text { text: t } => {
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(t);
                    }
                    ContentBlock::ToolUse {
                        id,
                        name,
                        input: args,
                        ..
                    } => {
                        if !text.is_empty() {
                            input.push(text_item(&message.role, &std::mem::take(&mut text)));
                        }
                        input.push(json!({
                            "type": "function_call",
                            // `call_id`, not the item id. The response carries
                            // both, and only this one may be echoed back.
                            "call_id": id,
                            "name": name,
                            // A JSON *string*, not an object.
                            "arguments": args.to_string(),
                        }));
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        if !text.is_empty() {
                            input.push(text_item(&message.role, &std::mem::take(&mut text)));
                        }
                        // There is no error flag on this item, and dropping the
                        // distinction would present a failure to the model as a
                        // result. The marker goes in the text instead.
                        let output = match is_error {
                            true => format!("[error] {content}"),
                            false => content.clone(),
                        };
                        input.push(json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": output,
                        }));
                    }
                }
            }
            if !text.is_empty() {
                input.push(text_item(&message.role, &text));
            }
        }
    }
}

/// A plain message item. The content type differs by direction: what the model
/// produced is `output_text`, what it is given is `input_text`.
fn text_item(role: &str, text: &str) -> Value {
    let content_type = match role {
        "assistant" => "output_text",
        _ => "input_text",
    };
    json!({
        "type": "message",
        "role": role,
        "content": [{ "type": content_type, "text": text }],
    })
}

/// A tool definition, in the flat Responses shape.
///
/// Not the Chat Completions `{type, function: {...}}` wrapper, which this route
/// does not accept.
fn tool_item(tool: &Tool) -> Value {
    json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.parameters,
        // Strict mode demands `additionalProperties: false` and every property
        // in `required` on every schema; Leviath's tool schemas do not all
        // carry that, and a strict rejection would be a hard 400 rather than a
        // looser validation.
        "strict": false,
    })
}

/// A cache key that is stable across a stage's turns and changes when the
/// prefix legitimately does.
///
/// Derived from the stable blocks rather than passed in, because the provider
/// is handed an `InferenceRequest` and nothing else. Hashing only the stable
/// tier is what makes it survive a turn: the sliding window grows every
/// iteration, and folding that in would mint a new key each time and defeat the
/// mechanism entirely.
///
/// Fixed width by construction, so the length limit is unreachable regardless
/// of what a region is named.
fn cache_key(request: &InferenceRequest) -> String {
    let mut hasher = DefaultHasher::new();
    request.model.hash(&mut hasher);
    for block in &request.system {
        if block.volatility == leviath_core::Volatility::Stable {
            block.region.hash(&mut hasher);
            block.text.hash(&mut hasher);
        }
    }
    format!("lev-{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests;
