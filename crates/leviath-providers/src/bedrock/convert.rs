//! Leviath's request and Bedrock's Converse body, each way.
//!
//! Converse is one shape for every vendor Bedrock carries, so most of this
//! is the same for a Claude, a Nova and a Llama. The vendor still shows
//! through in the corners: which models take a `cachePoint`, whether a tool
//! result may carry a `status`, and which reasoning field is theirs. Those
//! corners are decided here from the [`Vendor`], so the provider itself
//! never has to know a vendor exists.

use super::catalog::Vendor;
use crate::provider::{
    ContentBlock, FinishReason, InferenceRequest, InferenceResponse, Message, MessageContent,
    ModelCapabilities, ProviderError, Result, TokenUsage, ToolCall,
};
use serde_json::{Value, json};

/// The key the reasoning blob is stored under, so a blob another provider
/// wrote (Codex's sealed string) is never mistaken for this one.
pub(super) const REASONING_KEY: &str = "bedrock";

/// Keys of `extra` the body builder reads itself rather than passing on.
const CONSUMED_EXTRA: &[&str] = &["top_p", "stop", "stop_sequences", "tool_choice"];

/// The reasoning blocks of a turn as the opaque blob the runtime stores, or
/// `None` when the turn had none.
///
/// The blocks are kept exactly as Bedrock sent them (`reasoningText` with
/// its `signature`, or `redactedContent`), because that is what Claude wants
/// back on the next turn and nothing else can read them anyway.
pub(super) fn reasoning_blob(blocks: &[Value]) -> Option<String> {
    (!blocks.is_empty()).then(|| json!({ REASONING_KEY: blocks }).to_string())
}

/// The reasoning blocks in a stored blob, if this provider wrote it.
///
/// A blob that is not ours (Codex's, which is not JSON at all) is `None`,
/// and the turn is replayed without reasoning: the other provider's token
/// means nothing to Bedrock and sending it would be refused.
pub(super) fn replayable_reasoning(blob: &str) -> Option<Vec<Value>> {
    let value: Value = serde_json::from_str(blob).ok()?;
    let blocks = value.get(REASONING_KEY)?.as_array()?.clone();
    (!blocks.is_empty()).then_some(blocks)
}

/// Whether `extra` asks for a reasoning pass that takes the sampling
/// parameters off the table.
///
/// Claude refuses a temperature when `thinking` is enabled, and Nova 2
/// refuses temperature, topP and maxTokens at high effort. Read from what
/// the stage asked for, not the vendor: the same key means the same thing
/// on every model that accepts it.
fn reasoning_requested(extra: &Value) -> (bool, bool) {
    let thinking = extra
        .pointer("/thinking/type")
        .and_then(|v| v.as_str())
        .is_some_and(|t| t == "enabled" || t == "adaptive");
    let nova_high = extra
        .pointer("/reasoningConfig/maxReasoningEffort")
        .and_then(|v| v.as_str())
        == Some("high")
        && extra
            .pointer("/reasoningConfig/type")
            .and_then(|v| v.as_str())
            != Some("disabled");
    (thinking || nova_high, nova_high)
}

/// Whether a key of `extra` belongs to this vendor's request at all.
///
/// Bedrock validates `additionalModelRequestFields` against the model, so
/// Claude's `thinking` on a Nova request is a `ValidationException` rather
/// than an ignored field. The title lane sends `thinking: disabled` to every
/// Bedrock model, which is exactly the case.
fn passes_to_vendor(key: &str, vendor: Vendor) -> bool {
    match key {
        "thinking" | "anthropic_beta" => vendor == Vendor::Anthropic,
        "reasoningConfig" => vendor == Vendor::Nova,
        _ => true,
    }
}

/// The Converse request body for `request`.
///
/// `caps` is the provider's answer for the model, so the converter stays a
/// pure function of what it is handed.
pub(super) fn converse_body(
    request: &InferenceRequest,
    caps: &ModelCapabilities,
    vendor: Vendor,
) -> Value {
    // A tool block in the history is refused when no tool is defined, and
    // refused outright by a model that takes none: either way the story is
    // told in prose instead.
    let messages = match caps.supports_tools && !request.tools.is_empty() {
        true => request.messages.clone(),
        false => crate::flatten_tool_turns(request.messages.clone()),
    };
    let caches = matches!(vendor, Vendor::Anthropic | Vendor::Nova);
    let mut body = json!({
        "messages": converse_messages(&messages, vendor, caches),
    });

    let system = system_blocks(request, caches);
    if !system.is_empty() {
        body["system"] = Value::Array(system);
    }

    let (no_sampling, no_max_tokens) = reasoning_requested(&request.extra);
    let mut inference = serde_json::Map::new();
    if !no_max_tokens {
        inference.insert("maxTokens".to_string(), json!(request.max_tokens));
    }
    if caps.supports_temperature && !no_sampling {
        inference.insert(
            "temperature".to_string(),
            crate::provider::json_number(request.temperature),
        );
    }
    if !no_sampling && let Some(top_p) = request.extra.get("top_p") {
        inference.insert("topP".to_string(), top_p.clone());
    }
    if let Some(stops) = stop_sequences(&request.extra) {
        inference.insert("stopSequences".to_string(), stops);
    }
    body["inferenceConfig"] = Value::Object(inference);

    if caps.supports_tools
        && !request.tools.is_empty()
        && let Some(config) = tool_config(request, vendor == Vendor::Anthropic)
    {
        body["toolConfig"] = config;
    }

    let extra_fields: serde_json::Map<String, Value> = request
        .extra
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| !CONSUMED_EXTRA.contains(&key.as_str()))
        .filter(|(key, _)| passes_to_vendor(key, vendor))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if !extra_fields.is_empty() {
        body["additionalModelRequestFields"] = Value::Object(extra_fields);
    }
    body
}

/// The `system` array: every block with text, with a `cachePoint` after each
/// index the shared breakpoint chooser picks, for the vendors that take one.
fn system_blocks(request: &InferenceRequest, caches: bool) -> Vec<Value> {
    let breakpoints: Vec<usize> = match caches {
        true => crate::anthropic::system_cache_breakpoints(
            &request.system,
            crate::anthropic::MAX_SYSTEM_BREAKPOINTS,
        ),
        false => Vec::new(),
    };
    let mut out: Vec<Value> = Vec::new();
    for (index, block) in request.system.iter().enumerate() {
        if !block.text.trim().is_empty() {
            out.push(json!({ "text": block.text }));
        }
        // A marker chosen for a blank block still ends the prefix before it;
        // it just sits after whatever was last sent, and never twice.
        if breakpoints.contains(&index)
            && out
                .last()
                .is_some_and(|last| last.get("cachePoint").is_none())
        {
            out.push(cache_point());
        }
    }
    out
}

/// The marker that ends a cached prefix.
fn cache_point() -> Value {
    json!({ "cachePoint": { "type": "default" } })
}

/// `stopSequences` from `stop` (a string or a list) or `stop_sequences`.
fn stop_sequences(extra: &Value) -> Option<Value> {
    let value = extra.get("stop").or_else(|| extra.get("stop_sequences"))?;
    match value {
        Value::String(s) => Some(json!([s])),
        Value::Array(_) => Some(value.clone()),
        _ => None,
    }
}

/// The `toolConfig` object, or `None` when `tool_choice` says the model may
/// not call anything, which Converse has no spelling for other than
/// defining no tools.
///
/// `caches` is whether a `cachePoint` may follow the definitions, which only
/// Claude takes: Nova caches system and messages but refuses the key here
/// ("extraneous key [cachePoint] is not permitted", measured on Nova Micro).
fn tool_config(request: &InferenceRequest, caches: bool) -> Option<Value> {
    let mut tools: Vec<Value> = request
        .tools
        .iter()
        .map(|t| {
            json!({
                "toolSpec": {
                    "name": t.name,
                    "description": t.description,
                    "inputSchema": { "json": t.parameters },
                }
            })
        })
        .collect();
    let choice = request.extra.get("tool_choice");
    let choice = match choice {
        Some(Value::String(s)) if s == "none" => return None,
        Some(Value::String(s)) if s == "auto" => Some(json!({ "auto": {} })),
        Some(Value::String(s)) if s == "any" || s == "required" => Some(json!({ "any": {} })),
        Some(Value::String(s)) => Some(json!({ "tool": { "name": s } })),
        Some(other) => other
            .get("name")
            .or_else(|| other.pointer("/function/name"))
            .and_then(|n| n.as_str())
            .map(|name| json!({ "tool": { "name": name } })),
        None => None,
    };
    // Tool definitions are a stable prefix worth caching on the vendors that
    // take a marker there.
    if caches {
        tools.push(cache_point());
    }
    let mut config = json!({ "tools": tools });
    if let Some(choice) = choice {
        config["toolChoice"] = choice;
    }
    Some(config)
}

/// The `messages` array: every message as Converse blocks, consecutive
/// same-role messages merged, because Converse insists the roles alternate
/// and the assembler does not.
fn converse_messages(messages: &[Message], vendor: Vendor, caches: bool) -> Vec<Value> {
    let mut names = DocumentNames::default();
    let mut out: Vec<(String, Vec<Value>)> = Vec::new();
    let mut last_cached: Option<usize> = None;
    for message in messages {
        let blocks = message_blocks(message, vendor, &mut names);
        if blocks.is_empty() {
            continue;
        }
        let role = match message.role.as_str() {
            "assistant" => "assistant",
            _ => "user",
        };
        match out.last_mut() {
            Some((last_role, last_blocks)) if last_role == role => last_blocks.extend(blocks),
            _ => out.push((role.to_string(), blocks)),
        }
        if message.cache_breakpoint {
            last_cached = Some(out.len() - 1);
        }
    }
    if caches && let Some(index) = last_cached {
        out[index].1.push(cache_point());
    }
    // Converse insists the first turn is the user's. The runtime pads only a
    // conversation with no user turn at all, and Anthropic's own API takes a
    // leading assistant turn, so a history whose task lives in the system
    // prompt opens on the model's first tool call: measured live as a 400,
    // "A conversation must start with a user message".
    if out.first().is_none_or(|(role, _)| role.as_str() != "user") {
        out.insert(
            0,
            (
                "user".to_string(),
                vec![json!({ "text": crate::provider::OPENING_TURN })],
            ),
        );
    }
    out.into_iter()
        .map(|(role, content)| json!({ "role": role, "content": content }))
        .collect()
}

/// One message's blocks: its reasoning first (Claude replays the signed
/// block ahead of the tool use it led to), then its content.
fn message_blocks(message: &Message, vendor: Vendor, names: &mut DocumentNames) -> Vec<Value> {
    let mut blocks = Vec::new();
    if message.role == "assistant"
        && vendor == Vendor::Anthropic
        && let Some(blob) = &message.reasoning
    {
        match replayable_reasoning(blob) {
            Some(reasoning) => blocks.extend(reasoning),
            None => tracing::debug!("a reasoning token from another provider is not replayed"),
        }
    }
    match &message.content {
        MessageContent::Text(text) => {
            if !text.trim().is_empty() {
                blocks.push(json!({ "text": text }));
            }
        }
        MessageContent::Blocks(content) => {
            blocks.extend(
                content
                    .iter()
                    .filter_map(|block| converse_block(block, vendor, names)),
            );
        }
    }
    blocks
}

/// One content block as Converse takes it, or `None` for one with nothing
/// in it.
fn converse_block(
    block: &ContentBlock,
    vendor: Vendor,
    names: &mut DocumentNames,
) -> Option<Value> {
    match block {
        ContentBlock::Text { text } => (!text.trim().is_empty()).then(|| json!({ "text": text })),
        ContentBlock::ToolUse {
            id, name, input, ..
        } => {
            // Converse wants an object; see `tool_input_object`.
            let input = crate::provider::tool_input_object(input);
            Some(json!({ "toolUse": { "toolUseId": id, "name": name, "input": input } }))
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            // Only Claude takes a `status`; the others are told in words.
            let (text, status) = match (vendor, *is_error) {
                (Vendor::Anthropic, true) => (result_text(content), Some("error")),
                (Vendor::Anthropic, false) => (result_text(content), Some("success")),
                (_, true) => (format!("Error: {}", result_text(content)), None),
                (_, false) => (result_text(content), None),
            };
            let mut result = json!({
                "toolUseId": tool_use_id,
                "content": [{ "text": text }],
            });
            if let Some(status) = status {
                result["status"] = json!(status);
            }
            Some(json!({ "toolResult": result }))
        }
        ContentBlock::Mime {
            part, data, name, ..
        } => Some(mime_block(part, data, name.as_deref(), names)),
    }
}

/// A tool result's text, with something in it: Converse refuses an empty
/// text block, and a tool that printed nothing still finished.
fn result_text(content: &str) -> String {
    match content.trim().is_empty() {
        true => "(no output)".to_string(),
        false => content.to_string(),
    }
}

/// A mime block as Converse's `image` or `document`, or its stand-in text
/// for a type Converse has no block for or bytes that were never loaded.
fn mime_block(
    part: &leviath_core::mime::BlobRef,
    data: &str,
    name: Option<&str>,
    names: &mut DocumentNames,
) -> Value {
    if data.is_empty() {
        return json!({ "text": part.stand_in });
    }
    match crate::mime::family_of(&part.mime_type) {
        crate::mime::Family::Image => match image_format(part.mime_type.subtype()) {
            Some(format) => json!({
                "image": { "format": format, "source": { "bytes": data } }
            }),
            None => json!({ "text": part.stand_in }),
        },
        crate::mime::Family::Document => json!({
            "document": {
                "format": "pdf",
                "name": names.unique(name),
                "source": { "bytes": data },
            }
        }),
        crate::mime::Family::Audio | crate::mime::Family::Video | crate::mime::Family::Other => {
            json!({ "text": part.stand_in })
        }
    }
}

/// Converse's `format` word for an image subtype, for the four it takes.
fn image_format(subtype: &str) -> Option<&'static str> {
    match subtype.to_ascii_lowercase().as_str() {
        "png" => Some("png"),
        "jpeg" | "jpg" => Some("jpeg"),
        "gif" => Some("gif"),
        "webp" => Some("webp"),
        _ => None,
    }
}

/// The document names used so far in one request, because Converse wants
/// each one unique.
#[derive(Default)]
struct DocumentNames {
    used: Vec<String>,
}

impl DocumentNames {
    /// `name` made acceptable and unique: Converse allows letters, digits,
    /// spaces, hyphens, parentheses and brackets, with no run of spaces.
    fn unique(&mut self, name: Option<&str>) -> String {
        let cleaned: String = name
            .unwrap_or_default()
            .chars()
            .map(|c| match c.is_ascii_alphanumeric() || "-()[]".contains(c) {
                true => c,
                false => ' ',
            })
            .collect();
        let mut base = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
        if base.is_empty() {
            base = "document".to_string();
        }
        let mut candidate = base.clone();
        let mut n = 2;
        while self.used.contains(&candidate) {
            candidate = format!("{base} ({n})");
            n += 1;
        }
        self.used.push(candidate.clone());
        candidate
    }
}

/// A `stopReason` as a [`FinishReason`].
pub(super) fn parse_stop_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" => FinishReason::Complete,
        "tool_use" => FinishReason::ToolCall,
        "max_tokens" => FinishReason::TokenLimit,
        "stop_sequence" => FinishReason::Stop,
        other => {
            tracing::debug!(reason = other, "unrecognised stopReason from Bedrock");
            FinishReason::Unknown
        }
    }
}

/// A `usage` object as [`TokenUsage`].
///
/// Bedrock's `inputTokens` is the fresh figure: the two cache counts are
/// reported beside it, not inside it, the way Anthropic's own API does.
/// Reconciled with `totalTokens` so a vendor that counts differently can
/// only make the total larger.
pub(super) fn parse_usage(usage: Option<&Value>) -> TokenUsage {
    let count = |key: &str| {
        usage
            .and_then(|u| u.get(key))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize
    };
    TokenUsage::new(
        count("inputTokens"),
        count("cacheReadInputTokens"),
        count("cacheWriteInputTokens"),
        count("outputTokens"),
    )
    .with_reported_total(count("totalTokens"))
}

/// A buffered Converse response as the runtime's [`InferenceResponse`].
pub(super) fn parse_response(body: &Value) -> Result<InferenceResponse> {
    let message = body.pointer("/output/message").ok_or_else(|| {
        ProviderError::InvalidResponse("converse response carries no output.message".to_string())
    })?;
    let mut content = String::new();
    let mut tool_calls = Vec::new();
    let mut reasoning = Vec::new();
    for block in message
        .get("content")
        .and_then(|c| c.as_array())
        .into_iter()
        .flatten()
    {
        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
            content.push_str(text);
        } else if let Some(call) = block.get("toolUse") {
            tool_calls.push(ToolCall {
                id: call
                    .get("toolUseId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                name: call
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                arguments: call
                    .get("input")
                    .cloned()
                    .unwrap_or(Value::Object(serde_json::Map::new())),
                thought_signature: None,
            });
        } else if block.get("reasoningContent").is_some() {
            reasoning.push(block.clone());
        }
    }
    let stop_reason = body
        .get("stopReason")
        .and_then(|v| v.as_str())
        .unwrap_or("end_turn");
    Ok(InferenceResponse {
        content,
        tool_calls,
        tokens_used: parse_usage(body.get("usage")),
        finish_reason: parse_stop_reason(stop_reason),
        reasoning: reasoning_blob(&reasoning),
        parts: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{SystemBlock, Tool};

    fn caps() -> ModelCapabilities {
        ModelCapabilities::default()
    }

    fn user(text: &str) -> Message {
        Message {
            role: "user".to_string(),
            content: text.to_string().into(),
            cache_breakpoint: false,
            reasoning: None,
        }
    }

    fn assistant(blocks: Vec<ContentBlock>) -> Message {
        Message {
            role: "assistant".to_string(),
            content: MessageContent::Blocks(blocks),
            cache_breakpoint: false,
            reasoning: None,
        }
    }

    fn request(messages: Vec<Message>) -> InferenceRequest {
        InferenceRequest {
            system: Vec::new(),
            messages,
            model: "us.anthropic.claude-sonnet-5".to_string(),
            max_tokens: 100,
            temperature: 0.7,
            tools: Vec::new(),
            extra: Value::Null,
            request_timeout_secs: None,
        }
    }

    fn system(text: &str) -> SystemBlock {
        SystemBlock {
            text: text.to_string(),
            cache_hint: leviath_core::CacheHint::Always,
            region: String::new(),
            volatility: leviath_core::Volatility::Stable,
        }
    }

    fn tool() -> Tool {
        Tool {
            name: "read".to_string(),
            description: "Read a file".to_string(),
            parameters: json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        }
    }

    #[test]
    fn a_text_request_is_the_smallest_body() {
        let body = converse_body(&request(vec![user("hi")]), &caps(), Vendor::Anthropic);
        assert_eq!(
            body["messages"],
            json!([{ "role": "user", "content": [{ "text": "hi" }] }])
        );
        assert_eq!(
            body["inferenceConfig"],
            json!({ "maxTokens": 100, "temperature": 0.7 })
        );
        assert!(body.get("system").is_none());
        assert!(body.get("toolConfig").is_none());
        assert!(body.get("additionalModelRequestFields").is_none());
    }

    #[test]
    fn a_model_that_takes_no_temperature_is_not_sent_one() {
        let no_temp = ModelCapabilities {
            supports_temperature: false,
            ..caps()
        };
        let body = converse_body(&request(vec![user("hi")]), &no_temp, Vendor::Anthropic);
        assert_eq!(body["inferenceConfig"], json!({ "maxTokens": 100 }));
    }

    #[test]
    fn system_blocks_are_sent_in_order_with_a_cache_point_for_claude_only() {
        let mut req = request(vec![user("hi")]);
        req.system = vec![system(&"a".repeat(5000)), system("  "), system("b")];
        let body = converse_body(&req, &caps(), Vendor::Anthropic);
        let system = body["system"].as_array().unwrap();
        assert_eq!(system[0]["text"].as_str().unwrap().len(), 5000);
        assert_eq!(system[1], json!({ "cachePoint": { "type": "default" } }));
        assert_eq!(system[2], json!({ "text": "b" }));
        assert_eq!(system[3], json!({ "cachePoint": { "type": "default" } }));
        assert_eq!(system.len(), 4);
        let body = converse_body(&req, &caps(), Vendor::Meta);
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 2);
        assert!(system.iter().all(|b| b.get("cachePoint").is_none()));
    }

    #[test]
    fn sampling_and_stop_parameters_reach_inference_config() {
        let mut req = request(vec![user("hi")]);
        req.extra = json!({ "top_p": 0.9, "stop": "END", "top_k": 5 });
        let body = converse_body(&req, &caps(), Vendor::Meta);
        assert_eq!(body["inferenceConfig"]["topP"], json!(0.9));
        assert_eq!(body["inferenceConfig"]["stopSequences"], json!(["END"]));
        assert_eq!(body["additionalModelRequestFields"], json!({ "top_k": 5 }));
        req.extra = json!({ "stop_sequences": ["a", "b"] });
        let body = converse_body(&req, &caps(), Vendor::Meta);
        assert_eq!(body["inferenceConfig"]["stopSequences"], json!(["a", "b"]));
        req.extra = json!({ "stop": 7 });
        let body = converse_body(&req, &caps(), Vendor::Meta);
        assert!(body["inferenceConfig"].get("stopSequences").is_none());
    }

    #[test]
    fn thinking_takes_the_temperature_away_and_reaches_claude_only() {
        let mut req = request(vec![user("hi")]);
        req.extra =
            json!({ "thinking": { "type": "enabled", "budget_tokens": 1024 }, "top_p": 0.5 });
        let body = converse_body(&req, &caps(), Vendor::Anthropic);
        assert_eq!(body["inferenceConfig"], json!({ "maxTokens": 100 }));
        assert_eq!(
            body["additionalModelRequestFields"]["thinking"]["budget_tokens"],
            json!(1024)
        );
        // The same request to a Nova model: the field is not theirs.
        let body = converse_body(&req, &caps(), Vendor::Nova);
        assert!(body.get("additionalModelRequestFields").is_none());
        // Disabled thinking keeps the temperature.
        req.extra = json!({ "thinking": { "type": "disabled" } });
        let body = converse_body(&req, &caps(), Vendor::Anthropic);
        assert_eq!(body["inferenceConfig"]["temperature"], json!(0.7));
        req.extra = json!({ "anthropic_beta": ["context-1m-2025-08-07"] });
        let body = converse_body(&req, &caps(), Vendor::Anthropic);
        assert_eq!(
            body["additionalModelRequestFields"]["anthropic_beta"],
            json!(["context-1m-2025-08-07"])
        );
    }

    #[test]
    fn nova_high_effort_drops_every_sampling_parameter_and_the_cap() {
        let mut req = request(vec![user("hi")]);
        req.extra = json!({ "reasoningConfig": { "type": "enabled", "maxReasoningEffort": "high" }, "top_p": 0.5 });
        let body = converse_body(&req, &caps(), Vendor::Nova);
        assert_eq!(body["inferenceConfig"], json!({}));
        assert_eq!(
            body["additionalModelRequestFields"]["reasoningConfig"]["maxReasoningEffort"],
            json!("high")
        );
        req.extra =
            json!({ "reasoningConfig": { "type": "enabled", "maxReasoningEffort": "low" } });
        let body = converse_body(&req, &caps(), Vendor::Nova);
        assert_eq!(body["inferenceConfig"]["temperature"], json!(0.7));
        assert_eq!(body["inferenceConfig"]["maxTokens"], json!(100));
        // Not Claude's field.
        let body = converse_body(&req, &caps(), Vendor::Anthropic);
        assert!(body.get("additionalModelRequestFields").is_none());
        req.extra =
            json!({ "reasoningConfig": { "type": "disabled", "maxReasoningEffort": "high" } });
        let body = converse_body(&req, &caps(), Vendor::Nova);
        assert_eq!(body["inferenceConfig"]["temperature"], json!(0.7));
    }

    #[test]
    fn tools_become_tool_specs_with_the_choice_asked_for() {
        let mut req = request(vec![user("hi")]);
        req.tools = vec![tool()];
        let body = converse_body(&req, &caps(), Vendor::Meta);
        let config = &body["toolConfig"];
        assert_eq!(config["tools"][0]["toolSpec"]["name"], json!("read"));
        assert_eq!(
            config["tools"][0]["toolSpec"]["inputSchema"]["json"]["type"],
            json!("object")
        );
        assert_eq!(config["tools"].as_array().unwrap().len(), 1);
        assert!(config.get("toolChoice").is_none());
        // Claude gets a cache point after the definitions; Nova, which caches
        // elsewhere, refuses one there.
        let body = converse_body(&req, &caps(), Vendor::Anthropic);
        assert_eq!(
            body["toolConfig"]["tools"][1],
            json!({ "cachePoint": { "type": "default" } })
        );
        let body = converse_body(&req, &caps(), Vendor::Nova);
        assert_eq!(body["toolConfig"]["tools"].as_array().unwrap().len(), 1);
        for (choice, expected) in [
            (json!("auto"), json!({ "auto": {} })),
            (json!("any"), json!({ "any": {} })),
            (json!("required"), json!({ "any": {} })),
            (json!("read"), json!({ "tool": { "name": "read" } })),
            (
                json!({ "name": "read" }),
                json!({ "tool": { "name": "read" } }),
            ),
            (
                json!({ "type": "function", "function": { "name": "read" } }),
                json!({ "tool": { "name": "read" } }),
            ),
        ] {
            req.extra = json!({ "tool_choice": choice });
            let body = converse_body(&req, &caps(), Vendor::Meta);
            assert_eq!(body["toolConfig"]["toolChoice"], expected);
            assert!(body.get("additionalModelRequestFields").is_none());
        }
        req.extra = json!({ "tool_choice": { "unknown": true } });
        let body = converse_body(&req, &caps(), Vendor::Meta);
        assert!(body["toolConfig"].get("toolChoice").is_none());
        req.extra = json!({ "tool_choice": "none" });
        let body = converse_body(&req, &caps(), Vendor::Meta);
        assert!(body.get("toolConfig").is_none());
    }

    #[test]
    fn tool_turns_round_trip_with_the_status_claude_takes() {
        let history = vec![
            user("read it"),
            assistant(vec![
                ContentBlock::Text {
                    text: "Reading.".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "t1".to_string(),
                    name: "read".to_string(),
                    input: json!({ "path": "a.txt" }),
                    thought_signature: Some("gemini-only".to_string()),
                },
            ]),
            Message {
                role: "user".to_string(),
                content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: "contents".to_string(),
                    is_error: false,
                }]),
                cache_breakpoint: false,
                reasoning: None,
            },
        ];
        let mut req = request(history);
        req.tools = vec![tool()];
        let body = converse_body(&req, &caps(), Vendor::Anthropic);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[1]["content"][0], json!({ "text": "Reading." }));
        assert_eq!(
            messages[1]["content"][1],
            json!({ "toolUse": { "toolUseId": "t1", "name": "read", "input": { "path": "a.txt" } } })
        );
        assert!(!(body.to_string().contains("gemini-only")));
        assert_eq!(
            messages[2]["content"][0],
            json!({ "toolResult": { "toolUseId": "t1", "content": [{ "text": "contents" }], "status": "success" } })
        );
    }

    #[test]
    fn a_failed_or_empty_result_is_told_in_words_to_other_vendors() {
        let result = |content: &str, is_error: bool| Message {
            role: "user".to_string(),
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "t1".to_string(),
                content: content.to_string(),
                is_error,
            }]),
            cache_breakpoint: false,
            reasoning: None,
        };
        let mut req = request(vec![result("boom", true)]);
        req.tools = vec![tool()];
        let body = converse_body(&req, &caps(), Vendor::Meta);
        let block = &body["messages"][0]["content"][0]["toolResult"];
        assert_eq!(block["content"][0]["text"], json!("Error: boom"));
        assert!(block.get("status").is_none());
        req.messages = vec![result("", true)];
        let body = converse_body(&req, &caps(), Vendor::Anthropic);
        let block = &body["messages"][0]["content"][0]["toolResult"];
        assert_eq!(block["content"][0]["text"], json!("(no output)"));
        assert_eq!(block["status"], json!("error"));
        req.messages = vec![result("fine", false)];
        let body = converse_body(&req, &caps(), Vendor::Nova);
        let block = &body["messages"][0]["content"][0]["toolResult"];
        assert_eq!(block["content"][0]["text"], json!("fine"));
        assert!(block.get("status").is_none());
    }

    #[test]
    fn a_cut_off_tool_call_is_wrapped_rather_than_refused() {
        let mut req = request(vec![
            user("go"),
            assistant(vec![ContentBlock::ToolUse {
                id: "t1".to_string(),
                name: "read".to_string(),
                input: json!("{\"path\": \"a."),
                thought_signature: None,
            }]),
        ]);
        req.tools = vec![tool()];
        let body = converse_body(&req, &caps(), Vendor::Anthropic);
        assert_eq!(
            body["messages"][1]["content"][0]["toolUse"]["input"],
            json!({ "_raw": "{\"path\": \"a." })
        );
    }

    #[test]
    fn tool_history_is_flattened_when_no_tool_is_defined_or_taken() {
        let history = vec![
            user("go"),
            assistant(vec![ContentBlock::ToolUse {
                id: "t1".to_string(),
                name: "read".to_string(),
                input: json!({}),
                thought_signature: None,
            }]),
        ];
        let req = request(history.clone());
        let body = converse_body(&req, &caps(), Vendor::Anthropic);
        assert!(!(body.to_string().contains("toolUse")));
        assert!(body.get("toolConfig").is_none());
        let mut req = request(history);
        req.tools = vec![tool()];
        let no_tools = ModelCapabilities {
            supports_tools: false,
            ..caps()
        };
        let body = converse_body(&req, &no_tools, Vendor::DeepSeek);
        assert!(!(body.to_string().contains("toolUse")));
        assert!(body.get("toolConfig").is_none());
    }

    fn mime(mime_type: &str, data: &str, name: Option<&str>) -> ContentBlock {
        ContentBlock::Mime {
            part: leviath_core::mime::BlobRef {
                sha256: "abc".to_string(),
                mime_type: leviath_core::mime::MimeType::parse(mime_type).unwrap(),
                size: 3,
                width: None,
                height: None,
                duration_ms: None,
                tokens: 10,
                stand_in: format!("[{mime_type}]"),
            },
            data: data.to_string(),
            name: name.map(str::to_string),
            deliver: None,
            remote: None,
        }
    }

    fn user_blocks(blocks: Vec<ContentBlock>) -> Message {
        Message {
            role: "user".to_string(),
            content: MessageContent::Blocks(blocks),
            cache_breakpoint: false,
            reasoning: None,
        }
    }

    #[test]
    fn images_and_pdfs_become_their_blocks_and_the_rest_their_stand_in() {
        let req = request(vec![user_blocks(vec![
            mime("image/png", "AAAA", None),
            mime("image/jpg", "BBBB", None),
            mime("image/bmp", "CCCC", None),
            mime("application/pdf", "DDDD", Some("my report.pdf")),
            mime("application/pdf", "EEEE", Some("my report.pdf")),
            mime("application/pdf", "FFFF", Some("///")),
            mime("audio/mpeg", "GGGG", None),
            mime("image/png", "", None),
        ])]);
        let body = converse_body(&req, &caps(), Vendor::Anthropic);
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["image"]["format"], json!("png"));
        assert_eq!(content[0]["image"]["source"]["bytes"], json!("AAAA"));
        assert_eq!(content[1]["image"]["format"], json!("jpeg"));
        assert_eq!(content[2], json!({ "text": "[image/bmp]" }));
        assert_eq!(content[3]["document"]["format"], json!("pdf"));
        assert_eq!(content[3]["document"]["name"], json!("my report pdf"));
        assert_eq!(content[4]["document"]["name"], json!("my report pdf (2)"));
        assert_eq!(content[5]["document"]["name"], json!("document"));
        assert_eq!(content[6], json!({ "text": "[audio/mpeg]" }));
        assert_eq!(content[7], json!({ "text": "[image/png]" }));
    }

    #[test]
    fn consecutive_same_role_messages_merge_and_empty_ones_vanish() {
        let mut second = user("two");
        second.cache_breakpoint = true;
        let req = request(vec![
            user("one"),
            second,
            user("   "),
            Message {
                role: "system".to_string(),
                content: "late system".to_string().into(),
                cache_breakpoint: false,
                reasoning: None,
            },
            assistant(vec![ContentBlock::Text {
                text: "a".to_string(),
            }]),
            assistant(vec![ContentBlock::Text {
                text: "b".to_string(),
            }]),
        ]);
        let body = converse_body(&req, &caps(), Vendor::Anthropic);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0]["content"],
            json!([{ "text": "one" }, { "text": "two" }, { "text": "late system" }, { "cachePoint": { "type": "default" } }])
        );
        assert_eq!(
            messages[1]["content"],
            json!([{ "text": "a" }, { "text": "b" }])
        );
        // No cache point for a vendor that takes none.
        let body = converse_body(&req, &caps(), Vendor::Meta);
        assert!(!(body["messages"][0].to_string().contains("cachePoint")));
    }

    #[test]
    fn a_history_opening_on_the_assistant_gets_a_user_turn_first() {
        let req = request(vec![
            assistant(vec![ContentBlock::Text {
                text: "a".to_string(),
            }]),
            user("tool results"),
        ]);
        let body = converse_body(&req, &caps(), Vendor::Nova);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(
            messages[0],
            json!({ "role": "user", "content": [{ "text": "Begin." }] })
        );
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[2]["role"], "user");
        // A history that emptied out gets the same turn.
        let req = request(vec![user("   ")]);
        let body = converse_body(&req, &caps(), Vendor::Nova);
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        // One that opens on the user is left alone.
        let req = request(vec![user("hi")]);
        let body = converse_body(&req, &caps(), Vendor::Nova);
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
    }

    #[test]
    fn a_stored_reasoning_turn_is_replayed_ahead_of_its_tool_use_for_claude() {
        let blocks = vec![
            json!({ "reasoningContent": { "reasoningText": { "text": "hmm", "signature": "sig" } } }),
        ];
        let blob = reasoning_blob(&blocks).unwrap();
        assert_eq!(reasoning_blob(&[]), None);
        assert_eq!(replayable_reasoning(&blob), Some(blocks.clone()));
        assert_eq!(replayable_reasoning("not json"), None);
        assert_eq!(replayable_reasoning(r#"{"codex": "x"}"#), None);
        assert_eq!(replayable_reasoning(r#"{"bedrock": []}"#), None);
        assert_eq!(replayable_reasoning(r#"{"bedrock": "x"}"#), None);
        let mut turn = assistant(vec![
            ContentBlock::Text {
                text: "Reading.".to_string(),
            },
            ContentBlock::ToolUse {
                id: "t1".to_string(),
                name: "read".to_string(),
                input: json!({}),
                thought_signature: None,
            },
        ]);
        turn.reasoning = Some(blob);
        let mut req = request(vec![user("go"), turn.clone()]);
        req.tools = vec![tool()];
        let body = converse_body(&req, &caps(), Vendor::Anthropic);
        let content = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(content[0], blocks[0]);
        assert_eq!(content[1], json!({ "text": "Reading." }));
        assert!(content[2].get("toolUse").is_some());
        // Not for Nova, and not a blob another provider wrote.
        let body = converse_body(&req, &caps(), Vendor::Nova);
        assert!(
            body["messages"][1]["content"][0]
                .get("reasoningContent")
                .is_none()
        );
        let _guard = crate::test_support::always_on_tracing_guard();
        turn.reasoning = Some("sealed-codex-blob".to_string());
        req.messages = vec![user("go"), turn];
        let body = converse_body(&req, &caps(), Vendor::Anthropic);
        assert_eq!(
            body["messages"][1]["content"][0],
            json!({ "text": "Reading." })
        );
    }

    #[test]
    fn a_response_is_read_with_its_calls_reasoning_and_usage() {
        let body = json!({
            "output": { "message": { "role": "assistant", "content": [
                { "reasoningContent": { "reasoningText": { "text": "think", "signature": "s" } } },
                { "text": "Hello " },
                { "text": "there" },
                { "toolUse": { "toolUseId": "t1", "name": "read", "input": { "path": "a" } } },
                { "toolUse": { "name": "bare" } },
                { "somethingNew": {} }
            ] } },
            "stopReason": "tool_use",
            "usage": { "inputTokens": 10, "outputTokens": 5, "totalTokens": 18, "cacheReadInputTokens": 2, "cacheWriteInputTokens": 1 }
        });
        let response = parse_response(&body).unwrap();
        assert_eq!(response.content, "Hello there");
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].id, "t1");
        assert_eq!(response.tool_calls[0].arguments, json!({ "path": "a" }));
        assert_eq!(response.tool_calls[1].id, "");
        assert_eq!(response.tool_calls[1].arguments, json!({}));
        assert_eq!(response.finish_reason, FinishReason::ToolCall);
        assert_eq!(response.tokens_used.prompt_tokens, 10);
        assert_eq!(response.tokens_used.cached_tokens, 2);
        assert_eq!(response.tokens_used.cache_write_tokens, 1);
        assert_eq!(response.tokens_used.completion_tokens, 5);
        assert_eq!(response.tokens_used.total_tokens, 18);
        assert!(response.reasoning.unwrap().contains("\"signature\":\"s\""));
        assert!(response.parts.is_empty());
    }

    #[test]
    fn a_response_without_usage_or_stop_reason_still_reads() {
        let body = json!({ "output": { "message": { "content": [{ "text": "x" }] } } });
        let response = parse_response(&body).unwrap();
        assert_eq!(response.finish_reason, FinishReason::Complete);
        assert_eq!(response.tokens_used.total_tokens, 0);
        assert_eq!(response.reasoning, None);
        let body = json!({ "output": { "message": {} } });
        let response = parse_response(&body).unwrap();
        assert_eq!(response.content, "");
        let err = parse_response(&json!({ "output": {} })).unwrap_err();
        assert!(err.to_string().contains("no output.message"), "{err}");
    }

    #[test]
    fn every_stop_reason_is_named() {
        let _guard = crate::test_support::always_on_tracing_guard();
        assert_eq!(parse_stop_reason("end_turn"), FinishReason::Complete);
        assert_eq!(parse_stop_reason("tool_use"), FinishReason::ToolCall);
        assert_eq!(parse_stop_reason("max_tokens"), FinishReason::TokenLimit);
        assert_eq!(parse_stop_reason("stop_sequence"), FinishReason::Stop);
        assert_eq!(
            parse_stop_reason("guardrail_intervened"),
            FinishReason::Unknown
        );
        assert_eq!(parse_stop_reason("content_filtered"), FinishReason::Unknown);
    }

    #[test]
    fn a_reported_total_can_only_raise_the_derived_one() {
        let usage = parse_usage(Some(
            &json!({ "inputTokens": 4, "outputTokens": 4, "totalTokens": 5 }),
        ));
        assert_eq!(usage.total_tokens, 8);
        let usage = parse_usage(Some(&json!({ "totalTokens": 9 })));
        assert_eq!(usage.total_tokens, 9);
        assert_eq!(parse_usage(None).total_tokens, 0);
    }

    #[test]
    fn document_names_are_cleaned_and_kept_unique() {
        let mut names = DocumentNames::default();
        assert_eq!(
            names.unique(Some("Q3  report_(final)[v2].pdf")),
            "Q3 report (final)[v2] pdf"
        );
        assert_eq!(
            names.unique(Some("Q3  report_(final)[v2].pdf")),
            "Q3 report (final)[v2] pdf (2)"
        );
        assert_eq!(
            names.unique(Some("Q3  report_(final)[v2].pdf")),
            "Q3 report (final)[v2] pdf (3)"
        );
        assert_eq!(names.unique(None), "document");
        assert_eq!(names.unique(Some("")), "document (2)");
    }

    #[test]
    fn image_formats_are_the_four_converse_takes() {
        assert_eq!(image_format("PNG"), Some("png"));
        assert_eq!(image_format("jpg"), Some("jpeg"));
        assert_eq!(image_format("jpeg"), Some("jpeg"));
        assert_eq!(image_format("gif"), Some("gif"));
        assert_eq!(image_format("webp"), Some("webp"));
        assert_eq!(image_format("tiff"), None);
    }
}
