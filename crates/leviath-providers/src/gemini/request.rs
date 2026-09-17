//! Turning a Leviath request into a Gemini Interactions body.
//!
//! The conversation goes through the same repairs every chat-shaped route
//! gets (a call with no answer dropped, a call Gemini did not sign told as
//! text, a user turn ahead of every call turn, at least one user turn at all;
//! see `openai_compat::chat_messages`), with each stored part written as an
//! Interactions content item. The repaired messages then become steps:
//!
//! - `system` text becomes `system_instruction`;
//! - a `user` message becomes a `user_input` step;
//! - an assistant's text becomes a `model_output` step, and each call it made a
//!   `function_call` step, preceded by a `thought` step carrying the signature
//!   Gemini issued for it;
//! - a tool result becomes a `function_result` step naming its call.

use serde_json::{Map, Value, json};

use crate::openai_compat::{ToolArgsFormat, chat_messages};
use crate::provider::InferenceRequest;

/// Keys a stage's `[model.parameters]` may set that belong under
/// `generation_config` rather than at the top of the body.
const GENERATION_KEYS: &[&str] = &[
    "temperature",
    "top_p",
    "seed",
    "stop_sequences",
    "thinking_level",
    "thinking_summaries",
    "tool_choice",
    "max_output_tokens",
];

/// Top-level keys the route has no use for: Chat Completions names for what
/// the body already carries.
const DROPPED_KEYS: &[&str] = &["max_tokens", "max_completion_tokens", "stream_options"];

/// Build the body for `request`. `temperature` is sent only when the model
/// samples.
pub(crate) fn build(request: &InferenceRequest, temperature: bool) -> Value {
    let messages = chat_messages(request, ToolArgsFormat::Object, crate::mime::gemini_part);
    let mut system: Vec<String> = Vec::new();
    let mut names: std::collections::HashMap<String, String> = Default::default();
    let mut steps: Vec<Value> = Vec::new();
    for message in &messages {
        match message["role"].as_str().unwrap_or_default() {
            "system" => system.push(text_of(&message["content"])),
            "assistant" => push_assistant(&mut steps, &mut names, message),
            "tool" => {
                let call_id = message["tool_call_id"].as_str().unwrap_or_default();
                steps.push(json!({
                    "type": "function_result",
                    "name": names.get(call_id).cloned().unwrap_or_default(),
                    "call_id": call_id,
                    "result": message["content"].as_str().unwrap_or_default(),
                }));
            }
            _ => steps.push(json!({
                "type": "user_input",
                "content": content_items(&message["content"]),
            })),
        }
    }

    let mut generation = Map::new();
    generation.insert("max_output_tokens".into(), json!(request.max_tokens));
    if temperature {
        generation.insert(
            "temperature".into(),
            crate::provider::json_number(request.temperature),
        );
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert("input".into(), Value::Array(steps));
    let system = system.join("\n\n");
    if !system.trim().is_empty() {
        body.insert("system_instruction".into(), json!(system));
    }
    if !request.tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                        })
                    })
                    .collect(),
            ),
        );
    }
    // Stored interactions are kept 55 days on the paid tier, and nothing here
    // reads one back: every request carries its whole conversation.
    body.insert("store".into(), json!(false));
    body.insert("stream".into(), json!(true));

    merge_extra(&mut body, &mut generation, &request.extra);
    body.insert("generation_config".into(), Value::Object(generation));
    Value::Object(body)
}

/// A stage's parameters, merged in last so they win: generation settings go
/// under `generation_config`, a Chat Completions `reasoning_effort` becomes a
/// `thinking_level`, and everything else sits at the top.
fn merge_extra(body: &mut Map<String, Value>, generation: &mut Map<String, Value>, extra: &Value) {
    let Some(extra) = extra.as_object() else {
        return;
    };
    for (key, value) in extra {
        match key.as_str() {
            "generation_config" => {
                if let Some(fields) = value.as_object() {
                    generation.extend(fields.clone());
                }
            }
            "reasoning_effort" => {
                let level = match value.as_str() {
                    Some("none") => json!("minimal"),
                    _ => value.clone(),
                };
                generation.entry("thinking_level").or_insert(level);
            }
            k if GENERATION_KEYS.contains(&k) => {
                generation.insert(key.clone(), value.clone());
            }
            k if DROPPED_KEYS.contains(&k) => {}
            _ => {
                body.insert(key.clone(), value.clone());
            }
        }
    }
}

/// An assistant message's steps: its text, then each call behind its
/// signature.
fn push_assistant(
    steps: &mut Vec<Value>,
    names: &mut std::collections::HashMap<String, String>,
    message: &Value,
) {
    let text = text_of(&message["content"]);
    if !text.is_empty() {
        steps.push(json!({
            "type": "model_output",
            "content": [{ "type": "text", "text": text }],
        }));
    }
    for call in message["tool_calls"].as_array().into_iter().flatten() {
        let id = call["id"].as_str().unwrap_or_default();
        let name = call
            .pointer("/function/name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        names.insert(id.to_string(), name.to_string());
        if let Some(signature) = call
            .pointer("/extra_content/google/thought_signature")
            .and_then(Value::as_str)
        {
            steps.push(json!({ "type": "thought", "signature": signature }));
        }
        steps.push(json!({
            "type": "function_call",
            "id": id,
            "name": name,
            "arguments": call.pointer("/function/arguments").cloned().unwrap_or(json!({})),
        }));
    }
}

/// A chat message's content as plain text: a string as it is, a part list's
/// text parts joined.
fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// A chat message's content as Interactions content items. The parts are
/// already written in that shape by `mime::gemini_part`, and a text part is
/// spelled the same in both.
fn content_items(content: &Value) -> Value {
    match content {
        Value::Array(parts) => Value::Array(parts.clone()),
        other => json!([{ "type": "text", "text": text_of(other) }]),
    }
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;
