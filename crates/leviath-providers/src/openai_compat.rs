//! Shared request/response handling for OpenAI-compatible APIs.
//!
//! Used by OpenAI, Gemini, and OpenRouter providers that speak the
//! OpenAI Chat Completions format.

use crate::provider::{
    ContentBlock, InferenceRequest, InferenceResponse, MessageContent, ProviderError, Result,
    StreamChunk, TokenUsage, ToolCall, ToolCallDelta, check_http_response,
    parse_openai_finish_reason,
};
use crate::rate_limit::RateLimiter;
use futures_core::Stream;
use std::pin::Pin;

/// Send an OpenAI-compatible chat request and return the checked response.
///
/// Consolidates the send-and-handle lifecycle shared by every
/// OpenAI-compatible provider: optional `debug-http` request logging,
/// the `POST` with the given headers and JSON body, transport-error
/// mapping (with `debug-http` error logging), `debug-http` response
/// logging, `check_http_response`, and rate-limiter backoff reset on
/// success. Callers remain responsible for `limiter.acquire()` up front
/// and for consuming the returned `reqwest::Response` (parsing JSON and
/// recording tokens for `infer`, or `bytes_stream()` for `infer_stream`).
///
/// `headers` are applied both to the outgoing request and (feature-gated)
/// to the logged header map, so the wire request and the debug log stay
/// in sync.
pub async fn send_chat_request(
    client: &reqwest::Client,
    provider_name: &str,
    url: &str,
    headers: &[(&str, String)],
    body: &serde_json::Value,
    limiter: Option<&RateLimiter>,
    request_timeout_secs: Option<u64>,
) -> Result<reqwest::Response> {
    // Nothing outside the feature-gated logging below reads `provider_name`, so
    // without `debug-http` it is genuinely unused. Discarding that one binding
    // by name beats the function-wide `allow(unused_variables)` this replaced:
    // that attribute would equally have hidden a parameter someone stopped using
    // for real, and it covered a signature of seven.
    #[cfg(not(feature = "debug-http"))]
    let _ = provider_name;

    #[cfg(feature = "debug-http")]
    {
        let mut header_map = reqwest::header::HeaderMap::new();
        for (name, value) in headers {
            if let (Ok(header_name), Ok(header_value)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                value.parse::<reqwest::header::HeaderValue>(),
            ) {
                header_map.insert(header_name, header_value);
            }
        }
        let body_size = serde_json::to_vec(body).map(|b| b.len()).unwrap_or(0);
        crate::debug_http::log_request(provider_name, "POST", url, &header_map, body_size);
    }
    #[cfg(feature = "debug-http")]
    let start = std::time::Instant::now();

    let mut builder =
        crate::provider::apply_request_timeout(client.post(url), request_timeout_secs);
    for (name, value) in headers {
        builder = builder.header(*name, value);
    }

    // `ProviderError::transport` rather than the raw `e.to_string()` this used
    // to be. Every provider's `infer` and `infer_stream` comes through here, so
    // this one line is the whole inference path's classification: without it
    // `failure_kind()` is `None` for every timeout, reset and refused
    // connection a run ever hits, the message a paused run shows is `Display`
    // on a `reqwest::Error` - the same sentence for all four - and the circuit
    // breaker's patience for a provider that answered slowly (see
    // `CircuitPolicy::threshold_for`) can never be reached.
    let response = builder.json(body).send().await.map_err(|e| {
        #[cfg(feature = "debug-http")]
        crate::debug_http::log_error(provider_name, url, &e.to_string());
        ProviderError::transport("sending the request", &e)
    })?;

    #[cfg(feature = "debug-http")]
    crate::debug_http::log_response(
        provider_name,
        url,
        response.status().as_u16(),
        response.headers(),
        response.content_length(),
        start.elapsed(),
    );

    let response = check_http_response(response, limiter).await?;

    if let Some(limiter) = limiter {
        limiter.reset_backoff().await;
    }

    Ok(response)
}

/// Build the JSON request body for the OpenAI Chat Completions API.
/// Convert one message's content into one or more OpenAI-format messages. A
/// [`MessageContent::Blocks`] message carrying tool calls/results expands to
/// several (an assistant `tool_calls` message, or one `tool`-role message per
/// result), so tool history round-trips correctly on OpenAI-compatible APIs
/// instead of being serialized raw in Anthropic block form.
pub fn message_to_openai(role: &str, content: &MessageContent) -> Vec<serde_json::Value> {
    message_to_openai_with(role, content, ToolArgsFormat::JsonString)
}

/// [`message_to_openai`], naming how tool-call arguments are rendered.
pub fn message_to_openai_with(
    role: &str,
    content: &MessageContent,
    tool_args: ToolArgsFormat,
) -> Vec<serde_json::Value> {
    match content {
        MessageContent::Text(text) => {
            vec![serde_json::json!({ "role": role, "content": text })]
        }
        MessageContent::Blocks(blocks) => {
            let text_parts: Vec<&str> = blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let tool_results: Vec<(&str, &str)> = blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => Some((tool_use_id.as_str(), content.as_str())),
                    _ => None,
                })
                .collect();
            let tool_calls: Vec<serde_json::Value> = blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse {
                        id,
                        name,
                        input,
                        thought_signature,
                    } => {
                        let mut call = serde_json::json!({
                            "id": id,
                            "type": "function",
                            "function": { "name": name, "arguments": tool_args.render(input) }
                        });
                        // Echoed back exactly where the provider put it.
                        if let Some(sig) = thought_signature {
                            call["extra_content"] =
                                serde_json::json!({ "google": { "thought_signature": sig } });
                        }
                        Some(call)
                    }
                    _ => None,
                })
                .collect();

            // A block list can carry calls and results at once (a compacted
            // turn, or a stage that folded both into one entry). Emitting only
            // the calls silently dropped the results, leaving a function-call
            // turn with no response after it - which Gemini rejects outright.
            let mut out = Vec::new();
            if !tool_calls.is_empty() {
                let content = text_parts.join("");
                let mut msg_json = serde_json::json!({
                    "role": "assistant",
                    "tool_calls": tool_calls,
                });
                if !content.is_empty() {
                    msg_json["content"] = serde_json::Value::String(content);
                }
                out.push(msg_json);
            }
            out.extend(tool_results.iter().map(|(tool_use_id, content)| {
                serde_json::json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": content,
                })
            }));
            if out.is_empty() {
                out.push(serde_json::json!({ "role": role, "content": text_parts.join("") }));
            }
            out
        }
    }
}

/// The full OpenAI-format message array for a request: `request.system` blocks
/// prepended as `system`-role messages, then each conversation message
/// converted via [`message_to_openai`]. Reused by every OpenAI-compatible
/// provider so system prompts and tool history are handled uniformly.
pub fn openai_messages(request: &InferenceRequest) -> Vec<serde_json::Value> {
    openai_messages_with(request, ToolArgsFormat::JsonString)
}

/// [`openai_messages`], naming how tool-call arguments are rendered.
pub fn openai_messages_with(
    request: &InferenceRequest,
    tool_args: ToolArgsFormat,
) -> Vec<serde_json::Value> {
    let mut messages: Vec<serde_json::Value> = Vec::new();
    // One system message, however many blocks the context assembled into.
    //
    // A block per pinned region is how Leviath thinks about system content, and
    // Anthropic takes it that way natively. The OpenAI chat shape has no such
    // concept: it has *a* system message, and emitting several is at best
    // unusual. Some Ollama chat templates reject the second one outright -
    // qwen3.8 answers `HTTP 500 {"error":"system message must be at the
    // beginning"}`, which is a misleading way to say "at most one". An agent
    // with several pinned regions could not take a single turn against it.
    //
    // Empty blocks are dropped rather than joined, or a region that happens to
    // be empty this turn contributes a blank paragraph to the prompt.
    let system: Vec<&str> = request
        .system
        .iter()
        .map(|block| block.text.trim())
        .filter(|text| !text.is_empty())
        .collect();
    if !system.is_empty() {
        messages.push(serde_json::json!({
            "role": "system",
            "content": system.join("\n\n"),
        }));
    }
    for msg in &request.messages {
        messages.extend(message_to_openai_with(&msg.role, &msg.content, tool_args));
    }
    // After the unpaired sweep, not before: a call with no answer is dropped
    // outright, and folding it first would preserve it as text instead.
    let messages = drop_unpaired_tool_turns(messages);
    let messages = if wants_signed_tool_calls(&request.model) {
        fold_unsigned_tool_calls(messages)
    } else {
        messages
    };
    ensure_user_turn(satisfy_call_turn_order(messages))
}

/// Whether this model rejects a function call replayed without the signature it
/// would have issued.
///
/// Matched on the model rather than the provider: the same Gemini model is
/// reached natively and through a gateway, and it refuses either way. The name
/// carries a vendor prefix on some routes (`google/gemini-3.1-pro-preview`) and
/// not on others, so this looks for the family anywhere in the id.
fn wants_signed_tool_calls(model: &str) -> bool {
    model.contains("gemini")
}

/// Turn a built chat body into a streaming one.
///
/// Exists because `stream = true` on its own is a trap: an OpenAI-shaped API
/// reports no usage at all for a streamed call unless `stream_options` asks for
/// it, so a provider that sets one key and not the other bills every streamed
/// turn as free. OpenRouter did exactly that, and OpenRouter is the provider
/// where it costs most - its usage chunk carries the price the account was
/// actually charged, which for a model we hold no published rates for is the
/// only figure there is.
///
/// One function so the pairing is not something each provider has to remember.
/// Anthropic is deliberately not a caller: its own protocol reports usage on
/// `message_start` and `message_delta` without being asked, and
/// `stream_options` is not part of it.
pub fn make_streaming(body: &mut serde_json::Value) {
    body["stream"] = serde_json::Value::Bool(true);
    body["stream_options"] = serde_json::json!({ "include_usage": true });
}

/// The opaque per-call signature Gemini 3.x issues under `extra_content.google`
/// and then demands back verbatim on the turn that answers the call.
///
/// One reader for the buffered and the streamed shape, because they carry it in
/// the same place and getting it wrong in one of them is invisible until a run
/// makes a second turn: a call replayed without its signature is refused, and
/// the refusal names the field rather than the code path that dropped it.
fn thought_signature_of(call: &serde_json::Value) -> Option<String> {
    call.pointer("/extra_content/google/thought_signature")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Fold every unsigned function call, and the result that answered it, into the
/// assistant's own words.
///
/// A call this model did not sign cannot be replayed to it as a call. Removing
/// it alone would strand its result, and removing both would lose what the run
/// actually learned, so both are rewritten as text on the turn that made the
/// call. A signed call is left exactly as it is.
fn fold_unsigned_tool_calls(messages: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let signed = |call: &serde_json::Value| {
        call.pointer("/extra_content/google/thought_signature")
            .and_then(|v| v.as_str())
            .is_some_and(|sig| !sig.is_empty())
    };

    // What each call returned, so the fold can say so where the call was made.
    let mut answers: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for m in &messages {
        if m["role"] != "tool" {
            continue;
        }
        let id = m["tool_call_id"].as_str().unwrap_or_default();
        let body = m["content"].as_str().unwrap_or_default().to_string();
        answers.insert(id.to_string(), body);
    }

    let mut folded_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<serde_json::Value> = Vec::new();
    for mut m in messages {
        if m["role"] == "tool" {
            // Emitted as part of the assistant turn that called it, or kept if
            // its call survived as a call.
            let id = m["tool_call_id"].as_str().unwrap_or_default();
            if !folded_ids.contains(id) {
                out.push(m);
            }
            continue;
        }
        let Some(calls) = m["tool_calls"].as_array() else {
            out.push(m);
            continue;
        };
        let (keep, fold): (Vec<_>, Vec<_>) = calls.iter().cloned().partition(signed);
        if fold.is_empty() {
            out.push(m);
            continue;
        }

        let mut told = String::new();
        for call in &fold {
            let name = call
                .pointer("/function/name")
                .and_then(|v| v.as_str())
                .unwrap_or("a tool");
            let args = call
                .pointer("/function/arguments")
                .map(render_call_arguments)
                .unwrap_or_default();
            let id = call["id"].as_str().unwrap_or_default();
            let answer = answers.get(id).map(String::as_str).unwrap_or("");
            folded_ids.insert(id.to_string());
            // Every call reaching here was answered: the unpaired sweep ran
            // first. An answer that is empty reads as one, which is true.
            told.push_str(&format!(
                "\n\n[Earlier in this run I called {name}({args}) and it \
                 returned:\n{answer}]"
            ));
        }

        let text = m["content"].as_str().unwrap_or_default();
        m["content"] = serde_json::Value::String(format!("{text}{told}"));
        if keep.is_empty() {
            m.as_object_mut().map(|o| o.remove("tool_calls"));
        } else {
            m["tool_calls"] = serde_json::Value::Array(keep);
        }
        out.push(m);
    }
    out
}

/// A call's arguments as text, however this wire format rendered them.
///
/// [`ToolArgsFormat`] means they arrive here as either a JSON string or an
/// object, and the fold quotes them back to the model either way.
fn render_call_arguments(args: &serde_json::Value) -> String {
    match args.as_str() {
        Some(text) => text.to_string(),
        None => args.to_string(),
    }
}

/// The minimal user turn this layer inserts when a wire format demands one that
/// the conversation does not have.
///
/// Names where the real instruction is, so the addition cannot read as a new
/// request from the person, and is a fixed string at a fixed position so it
/// costs nothing in prompt-cache stability.
fn stand_in_user_turn() -> serde_json::Value {
    serde_json::json!({
        "role": "user",
        "content": "Proceed with the task described in the system instructions.",
    })
}

/// Ensure the conversation contains at least one user turn.
///
/// Leviath's task lives in a pinned context region, so it assembles into the
/// system prompt and a request can legitimately carry no user-role message at
/// all: on the very first inference, when the window holds nothing but assistant
/// prose, or when the only entries were tool responses whose calls have aged out
/// and been dropped above.
///
/// Anthropic and OpenAI accept that. Ollama with a Qwen 3.x template does not -
/// it answers `HTTP 500 {"error":"no user query found in messages"}` and the run
/// dies on a wire-format detail no agent author can see. Measured against
/// `qwen3.8-32k`: every shape carrying a user turn anywhere is accepted, and
/// every shape without one is refused, including `[system, assistant]`.
///
/// [`satisfy_call_turn_order`] already covers the histories that contain a tool
/// call, since it puts a user turn ahead of the first one. This covers the rest,
/// so the guarantee holds for every shape rather than for the common one.
///
/// The turn goes directly after the system message, where an opening request
/// belongs, rather than at the end where it would read as a fresh instruction
/// arriving after the assistant's last word.
fn ensure_user_turn(mut messages: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    if messages.iter().any(|m| m["role"] == "user") {
        return messages;
    }
    let after_system = usize::from(messages.first().is_some_and(|m| m["role"] == "system"));
    messages.insert(after_system, stand_in_user_turn());
    messages
}

/// Ensure every function-call turn follows a user turn or a function response
/// turn, per Gemini's validation rule.
///
/// Two shapes violate it in practice. Leviath's task lives in a pinned context
/// region, which assembles into the system prompt, so a run's first *message*
/// is the assistant's opening tool call. And mid-run, an assistant text turn
/// (a carried stage response, a nudge reply) can directly precede the next
/// call turn. Anthropic accepts both; Gemini answers HTTP 400 ("Please ensure
/// that function call turn comes immediately after a user turn or after a
/// function response turn") and the run dies on a wire-format detail no agent
/// author can see. The turn order is a wire-format expectation, so this layer
/// satisfies it rather than reshaping how regions assemble: a minimal user
/// turn is inserted ahead of each offending call turn, naming where the real
/// instruction is so the addition cannot read as a new request.
fn satisfy_call_turn_order(messages: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::with_capacity(messages.len());
    for msg in messages {
        let is_call_turn = msg["tool_calls"].as_array().is_some_and(|c| !c.is_empty());
        if is_call_turn {
            let prev_role = out.last().and_then(|m| m["role"].as_str());
            if !matches!(prev_role, Some("user") | Some("tool")) {
                out.push(stand_in_user_turn());
            }
        }
        out.push(msg);
    }
    out
}

/// Remove function-call turns whose responses are missing, and responses whose
/// call is missing.
///
/// A context window evicts entries independently, so a long run can assemble an
/// assistant `tool_calls` turn whose `tool` response has aged out (or the
/// reverse). Anthropic tolerates the gap; Gemini answers HTTP 400 - "Please
/// ensure that function call turn comes immediately after a user turn or after
/// a function response turn" - and the whole run dies on a wire-format detail
/// no agent author can see. Sending a conversation that is internally
/// consistent is this layer's job, so an unpaired turn is dropped rather than
/// forwarded.
fn drop_unpaired_tool_turns(messages: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let responded: std::collections::HashSet<&str> = messages
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["tool_call_id"].as_str())
        .collect();
    let called: std::collections::HashSet<&str> = messages
        .iter()
        .filter_map(|m| m["tool_calls"].as_array())
        .flatten()
        .filter_map(|c| c["id"].as_str())
        .collect();

    let keep: Vec<bool> = messages
        .iter()
        .map(|m| match m["tool_calls"].as_array() {
            // An assistant call turn survives only if every call it makes was
            // answered: a partially answered turn is the same broken shape.
            Some(calls) => calls
                .iter()
                .filter_map(|c| c["id"].as_str())
                .all(|id| responded.contains(id)),
            None => match (m["role"].as_str(), m["tool_call_id"].as_str()) {
                (Some("tool"), Some(id)) => called.contains(id),
                _ => true,
            },
        })
        .collect();

    messages
        .into_iter()
        .zip(keep)
        .filter_map(|(m, keep)| keep.then_some(m))
        .collect()
}

/// How a server wants a prior tool call's arguments replayed.
///
/// The second place the dialect forked. OpenAI carries `arguments` as a
/// *JSON-encoded string*; Ollama's Go server declares the field as
/// `api.ToolCallFunctionArguments` and rejects the string form outright:
///
/// ```text
/// json: cannot unmarshal string into Go struct field
/// ChatRequest.messages.tool_calls.function.arguments
/// ```
///
/// The first call of a turn therefore succeeded and the *second* failed, once
/// there was history to replay - which reads exactly like a model that is bad
/// at tool use, and is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolArgsFormat {
    /// A JSON-encoded string. OpenAI, OpenRouter, Gemini's compat endpoint.
    JsonString,
    /// A JSON object. Ollama.
    Object,
}

impl ToolArgsFormat {
    /// Render `input` the way this dialect expects it.
    pub fn render(self, input: &serde_json::Value) -> serde_json::Value {
        match self {
            Self::JsonString => serde_json::Value::String(input.to_string()),
            Self::Object => input.clone(),
        }
    }
}

/// Which key an OpenAI-dialect server expects the output-token cap under.
///
/// The dialect forked. OpenAI itself now *rejects* `max_tokens` on every current
/// model with `HTTP 400 unsupported_parameter`, while OpenRouter and Gemini's
/// compatibility endpoint still take it - so the field cannot simply be renamed
/// for everyone without breaking the two that work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenLimitField {
    /// `max_tokens`: the original spelling, and what every compatibility server
    /// in this workspace other than OpenAI accepts.
    MaxTokens,
    /// `max_completion_tokens`: what OpenAI requires.
    MaxCompletionTokens,
}

impl TokenLimitField {
    /// The JSON key this variant writes.
    pub fn key(self) -> &'static str {
        match self {
            Self::MaxTokens => "max_tokens",
            Self::MaxCompletionTokens => "max_completion_tokens",
        }
    }
}

/// Render a request as an OpenAI chat-completions body.
///
/// Shared by every provider speaking that dialect - OpenAI itself, OpenRouter,
/// and any `base_url` pointed at a compatible server - so one change to the wire
/// shape reaches all of them rather than three copies drifting apart.
///
/// Uses [`TokenLimitField::MaxTokens`], which is what a compatibility server
/// expects; OpenAI itself goes through [`build_openai_request_body_with`].
pub fn build_openai_request_body(request: &InferenceRequest) -> serde_json::Value {
    build_openai_request_body_with(request, TokenLimitField::MaxTokens)
}

/// [`build_openai_request_body`], naming the output-cap key explicitly.
pub fn build_openai_request_body_with(
    request: &InferenceRequest,
    token_limit: TokenLimitField,
) -> serde_json::Value {
    let messages = openai_messages(request);

    let mut body = serde_json::json!({
        "model": request.model,
        "temperature": crate::provider::json_number(request.temperature),
        "messages": messages,
    });
    body[token_limit.key()] = serde_json::json!(request.max_tokens);

    if !request.tools.is_empty() {
        let tools: Vec<serde_json::Value> = request
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
        body["tools"] = serde_json::Value::Array(tools);
    }

    merge_extra_params(
        body.as_object_mut()
            .expect("an OpenAI request body is always a JSON object"),
        &request.extra,
    );
    body
}

/// Whether an API error is OpenAI refusing tools *because* a reasoning effort
/// is in play, which it resolves by being sent `reasoning_effort: "none"`.
///
/// The current reasoning models reject function tools together with a reasoning
/// effort on `/v1/chat/completions`, and they apply an effort by default - so a
/// request that never mentions `reasoning_effort` is refused for a field it did
/// not set. The error says exactly that, and names the remedy.
///
/// Keyed on what the API said rather than on which model was asked, and
/// deliberately: a model list is what failed here. Nothing in this crate knew
/// `gpt-5.6` existed, and nothing should have to before it works.
///
/// The pairing is what makes this precise. A model that supports a reasoning
/// effort but not the value `none` reports a different problem in a message
/// that never mentions tools, and retrying *that* with `none` would resend the
/// same rejection.
pub(crate) fn tools_refused_over_reasoning_effort(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    detail.contains("reasoning_effort") && detail.contains("function tool")
}

/// Whether the API refused the request over the temperature we sent.
///
/// Some models take only their default temperature and reject any other value
/// outright:
///
/// ```text
/// Unsupported value: 'temperature' does not support 0.7 with this model.
/// Only the default (1) value is supported.
/// ```
///
/// The capability table said `gpt-5.5` supports temperature, because it matches
/// the `gpt-5` family branch and the rest of that family does. It does not, and
/// a research run died mid-`analyze` over it after 37 iterations and 2.4M
/// tokens. The table is now right about that model, but a table is the wrong
/// mechanism to rely on: the next model to do this will be wrong in it too,
/// on the day it ships. The API already says so, so ask it rather than a list.
pub(crate) fn temperature_refused(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    detail.contains("temperature") && detail.contains("does not support")
}

/// Merge a request's pass-through `extra` params (the manifest's
/// `[model.parameters]` beyond temperature/max_output_tokens - `top_p`, `stop`,
/// `seed`, `frequency_penalty`, …) into an OpenAI-shaped request `target`.
/// A non-object `extra` (e.g. `Null` when none are set) is a no-op, and keys the
/// builder already set are not overwritten (explicit request fields win).
pub fn merge_extra_params(
    target: &mut serde_json::Map<String, serde_json::Value>,
    extra: &serde_json::Value,
) {
    if let serde_json::Value::Object(params) = extra {
        for (key, value) in params {
            target.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
}

/// Parse a Chat Completions response body into an `InferenceResponse`.
/// Turn an OpenAI-style `error` envelope into the right [`ProviderError`].
///
/// Split out because it is reached from three places that never see a status
/// code: a 200 response whose body is an error, an SSE chunk carrying one, and
/// the truncated-stream case. The code inside the envelope is the status the
/// upstream provider used, so it feeds [`UnavailableReason::classify`] exactly
/// as a real status line would - which is what lets a 402 delivered this way
/// still fail over and trip the circuit breaker instead of killing the run.
pub(crate) fn openai_error_envelope(err: &serde_json::Value) -> ProviderError {
    let message = err
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    // OpenRouter sends `code` as a number; other gateways send the string form.
    let code = err
        .get("code")
        .and_then(|c| {
            c.as_u64()
                .or_else(|| c.as_str().and_then(|s| s.parse::<u64>().ok()))
        })
        .and_then(|c| u16::try_from(c).ok())
        .unwrap_or(0);

    let detail = match (code, message) {
        (0, "") => err.to_string(),
        (0, m) => m.to_string(),
        (c, "") => format!("HTTP {c}: {err}"),
        (c, m) => format!("HTTP {c}: {m}"),
    };

    // `classify` reads the body when the code alone is innocent, so a 0 here
    // still catches a credits message the gateway reported as a 200.
    match crate::provider::UnavailableReason::classify(code, message) {
        Some(reason) => ProviderError::Unavailable { reason, detail },
        None => ProviderError::ApiError(detail),
    }
}

/// The reasoning text of a message, when it carries any.
///
/// Two shapes are in the wild: a flat `reasoning` string, and the
/// `reasoning_details` array OpenRouter uses to preserve per-block structure.
/// Prefers the flat field and falls back to joining the array's text blocks.
fn reasoning_text(message: &serde_json::Value) -> Option<String> {
    if let Some(text) = message.get("reasoning").and_then(|v| v.as_str())
        && !text.trim().is_empty()
    {
        return Some(text.to_string());
    }
    let joined: String = message
        .get("reasoning_details")?
        .as_array()?
        .iter()
        .filter_map(|d| d.get("text").and_then(|v| v.as_str()))
        .collect::<Vec<_>>()
        .join("");
    (!joined.trim().is_empty()).then_some(joined)
}

/// Read an OpenAI chat-completions response back into an [`InferenceResponse`].
///
/// The counterpart to [`build_openai_request_body`], and shared for the same
/// reason. Handles the gateway case where a `200` body carries an `error`
/// object: see the comment inside for what that cost before it was unpacked.
pub fn parse_openai_response(body: &serde_json::Value) -> Result<InferenceResponse> {
    // A gateway does not always use the status line to report a failure.
    // OpenRouter answers 200 with `{"error":{"code":…,"message":…}}` when an
    // upstream provider rejects a request it had already accepted, and reading
    // that as "No choices in response" threw away the one field that says what
    // actually went wrong. Unpack it before looking for choices.
    if let Some(err) = body.get("error").filter(|e| !e.is_null()) {
        return Err(openai_error_envelope(err));
    }

    let choice = body
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .ok_or_else(|| ProviderError::InvalidResponse("No choices in response".to_string()))?;

    let message = choice
        .get("message")
        .ok_or_else(|| ProviderError::InvalidResponse("No message in choice".to_string()))?;

    let mut content = message
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();

    let mut tool_calls = Vec::new();
    if let Some(tcs) = message.get("tool_calls").and_then(|tc| tc.as_array()) {
        for tc in tcs {
            let id = tc
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let function = tc.get("function").unwrap_or(&serde_json::Value::Null);
            let name = function
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let arguments_str = function
                .get("arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("{}");
            let arguments: serde_json::Value = serde_json::from_str(arguments_str)
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
            let thought_signature = thought_signature_of(tc);
            tool_calls.push(ToolCall {
                id,
                name,
                arguments,
                thought_signature,
            });
        }
    }

    // Reasoning models answer with `content: null` and put their text under
    // `reasoning`, so reading `content` alone handed the runtime an empty
    // response: the agent got nudged to use its tools, looped, and the run
    // finished having said nothing. Only used when the message is otherwise
    // empty - a response carrying tool calls is not empty, and its reasoning
    // is working-out rather than output.
    if content.trim().is_empty() && tool_calls.is_empty() {
        content = reasoning_text(message).unwrap_or_default();
    }

    let usage = body.get("usage");
    let prompt_tokens = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    let completion_tokens = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;

    let finish_reason = choice
        .get("finish_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("stop");

    let cached_tokens = usage
        .and_then(|u| u.get("prompt_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    // Reported alongside `cached_tokens` by gateways that front a provider
    // charging a write premium - OpenRouter does for Anthropic models. It was
    // hardcoded to zero, so a run that paid the 1.25x write rate recorded none
    // of it and its token accounting understated what it cost.
    let cache_write_tokens = usage
        .and_then(|u| u.get("prompt_tokens_details"))
        .and_then(|d| d.get("cache_write_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;
    // What the gateway says this call actually cost. OpenRouter reports it in
    // USD when usage accounting is on; plain OpenAI omits the field entirely,
    // which is the `None` case and leaves the caller to fall back to rates.
    //
    // Taken verbatim and never recomputed: this is the figure that reflects
    // whatever the account actually pays - negotiated rates, promotional
    // pricing, the gateway's own margin, a model rerouted to a different
    // backend mid-request - none of which is visible from a published rate card.
    let reported_cost_usd = usage.and_then(|u| u.get("cost")).and_then(|v| v.as_f64());

    Ok(InferenceResponse {
        content,
        tool_calls,
        // The OpenAI shape reports a `prompt_tokens` that INCLUDES its
        // `prompt_tokens_details` breakdown, where Anthropic reports the three
        // separately. `TokenUsage::prompt_tokens` is the fresh figure, so the
        // breakdown comes back out here - otherwise cached tokens are counted
        // once at the full input rate and again at the cache rate, and the same
        // arithmetic cannot be right for both providers.
        //
        // Saturating: a gateway whose details exceed its own total is
        // malformed, and clamping to zero is better than wrapping to a
        // nonsensical figure.
        tokens_used: TokenUsage::new(
            prompt_tokens
                .saturating_sub(cached_tokens)
                .saturating_sub(cache_write_tokens),
            cached_tokens,
            cache_write_tokens,
            completion_tokens,
        )
        .with_reported_cost(reported_cost_usd),
        finish_reason: parse_openai_finish_reason(finish_reason),
    })
}

// SSE stream parser for OpenAI-compatible streaming APIs.
//
// The inner byte stream is boxed as a trait object rather than kept generic.
// In production this is always `reqwest`'s `bytes_stream()`; tests inject
// dozens of distinct mock stream types via `new`'s generic parameter, and a
// generic `impl<S> Stream` causes `cargo llvm-cov` to instrument each
// monomorphized `poll_next` separately, leaving some artificially "uncovered"
// even though the shared logic is fully exercised. Boxing collapses all of
// that into a single concrete `poll_next` implementation.
/// SSE stream wrapper that parses OpenAI-compatible server-sent events.
pub struct OpenAiSseStream {
    inner: Pin<Box<dyn Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send>>,
    buffer: String,
}

impl OpenAiSseStream {
    /// Create a new SSE stream wrapper around a byte stream.
    pub fn new<S>(inner: S) -> Self
    where
        S: Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    {
        Self {
            inner: Box::pin(inner),
            buffer: String::new(),
        }
    }
}

impl Stream for OpenAiSseStream {
    type Item = Result<StreamChunk>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            // Check for complete SSE events
            if let Some(chunk) = parse_openai_sse_event(&mut this.buffer) {
                return std::task::Poll::Ready(chunk);
            }

            match this.inner.as_mut().poll_next(cx) {
                std::task::Poll::Ready(Some(Ok(bytes))) => {
                    if let Ok(text) = std::str::from_utf8(&bytes) {
                        this.buffer.push_str(text);
                    }
                }
                std::task::Poll::Ready(Some(Err(e))) => {
                    return std::task::Poll::Ready(Some(Err(ProviderError::transport(
                        "reading the response stream",
                        &e,
                    ))));
                }
                std::task::Poll::Ready(None) => {
                    if let Some(chunk) = parse_openai_sse_event(&mut this.buffer) {
                        return std::task::Poll::Ready(chunk);
                    }
                    return std::task::Poll::Ready(None);
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

/// Parse a single SSE event from the buffer.
///
/// Returns `Some(Some(Ok(chunk)))` for data, `Some(Some(Err(..)))` for an error
/// the gateway delivered inside the stream, `Some(None)` for stream end, and
/// `None` when the buffer does not yet hold a complete event.
///
/// The error case exists because a stream is where OpenRouter reports a failure
/// it only discovered after committing to a 200: the upstream provider goes
/// down mid-generation, or the account runs out of credits between chunks. That
/// arrives as a `data:` line whose object is `{"error":{…}}` and no `choices`,
/// which is why it needs an arm of its own: matched only on `choices` it fits
/// the usage-only shape, and falling through there ends the stream cleanly -
/// a truncated answer with nothing anywhere saying why.
pub fn parse_openai_sse_event(buffer: &mut String) -> Option<Option<Result<StreamChunk>>> {
    // `None` until the double newline that terminates an event has arrived;
    // the caller polls again with more bytes.
    let (event_text, rest) = buffer.split_once("\n\n")?;
    let event_text = event_text.to_string();
    *buffer = rest.to_string();

    for line in event_text.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            let data = data.trim();
            if data == "[DONE]" {
                return Some(None); // Stream finished
            }

            let json: serde_json::Value = match serde_json::from_str(data) {
                Ok(j) => j,
                Err(_) => continue,
            };

            if let Some(err) = json.get("error").filter(|e| !e.is_null()) {
                return Some(Some(Err(openai_error_envelope(err))));
            }

            let choice = json
                .get("choices")
                .and_then(|c| c.as_array())
                .and_then(|c| c.first());

            // Handle usage-only chunk (no choices)
            if choice.is_none() {
                if let Some(usage) = json.get("usage") {
                    let prompt_tokens = usage
                        .get("prompt_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as usize;
                    let completion_tokens = usage
                        .get("completion_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as usize;
                    let cached_tokens = usage
                        .get("prompt_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as usize;
                    let cache_write_tokens = usage
                        .get("prompt_tokens_details")
                        .and_then(|d| d.get("cache_write_tokens"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as usize;
                    return Some(Some(Ok(StreamChunk {
                        delta: String::new(),
                        tool_calls: Vec::new(),
                        // Same normalisation as the non-streaming path: the
                        // details come back out of `prompt_tokens` so the three
                        // input counts stay disjoint.
                        tokens: Some(
                            TokenUsage::new(
                                prompt_tokens
                                    .saturating_sub(cached_tokens)
                                    .saturating_sub(cache_write_tokens),
                                cached_tokens,
                                cache_write_tokens,
                                completion_tokens,
                            )
                            // And the same cost passthrough, which this arm was
                            // missing. A choice-less usage chunk is exactly how
                            // OpenRouter reports what it charged, so the one
                            // shape that carries a real price was the one that
                            // dropped it.
                            .with_reported_cost(usage.get("cost").and_then(|v| v.as_f64())),
                        ),
                        finish_reason: None,
                    })));
                }
                continue;
            }

            let choice = choice.unwrap();
            let delta = choice.get("delta").unwrap_or(&serde_json::Value::Null);

            let content = delta
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();

            let mut tool_call_deltas = Vec::new();
            if let Some(tcs) = delta.get("tool_calls").and_then(|tc| tc.as_array()) {
                for tc in tcs {
                    let index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    let id = tc.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
                    let function = tc.get("function");
                    let name = function
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let args = function
                        .and_then(|f| f.get("arguments"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    tool_call_deltas.push(ToolCallDelta {
                        index,
                        id,
                        name,
                        arguments_delta: args.to_string(),
                        // Arrives on the delta that opens the call, beside the
                        // id and the name, and read the same way the buffered
                        // path reads it.
                        thought_signature: thought_signature_of(tc),
                    });
                }
            }

            let finish_reason = choice
                .get("finish_reason")
                .and_then(|v| v.as_str())
                .map(parse_openai_finish_reason);

            // Check for usage in the chunk
            let tokens = json.get("usage").map(|usage| {
                let pt = usage
                    .get("prompt_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                let ct = usage
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                let cached = usage
                    .get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                let written = usage
                    .get("prompt_tokens_details")
                    .and_then(|d| d.get("cache_write_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                // Same normalisation and the same cost passthrough as the
                // buffered path, so streaming a request does not report it
                // differently from buffering it.
                TokenUsage::new(
                    pt.saturating_sub(cached).saturating_sub(written),
                    cached,
                    written,
                    ct,
                )
                .with_reported_cost(usage.get("cost").and_then(|v| v.as_f64()))
            });

            return Some(Some(Ok(StreamChunk {
                delta: content,
                tool_calls: tool_call_deltas,
                tokens,
                finish_reason,
            })));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    /// The inference path classifies its own failures, driven *through*
    /// `send_chat_request` rather than around it.
    ///
    /// Every provider's `infer` and `infer_stream` goes through this one
    /// function, and until this test nothing checked it: the classification
    /// tests all went in by `list_models`, which is a different door. So
    /// `failure_kind()` was `None` for every timeout and every reset a run ever
    /// hit, and both things that read it downstream ran on the unclassified
    /// default - the sentence a parked run shows a person, and the circuit
    /// breaker's extra patience for a provider that is slow rather than dead.
    ///
    /// A server that accepts the connection and then writes nothing, because
    /// that is the failure this has to get right: it is the one that means the
    /// provider is *there*, and telling it from a provider that is not there is
    /// the whole reason `FailureKind` exists.
    #[tokio::test]
    async fn a_failed_send_says_what_kind_of_failure_it_was() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
        let addr = listener.local_addr().expect("has an address");
        std::thread::spawn(move || {
            let _held = listener.accept();
            std::thread::sleep(std::time::Duration::from_secs(30));
        });

        let client = crate::provider::build_http_client(Some(1)).expect("a client builds");
        let err = super::send_chat_request(
            &client,
            "test",
            &format!("http://{addr}/v1/chat/completions"),
            &[],
            &serde_json::json!({ "model": "m" }),
            None,
            None,
        )
        .await
        .expect_err("nothing ever answers");

        assert_eq!(
            err.failure_kind(),
            Some(crate::FailureKind::Timeout),
            "the inference path must say what went wrong, not just that something did"
        );
        // The remedy travels with it, so the person reading a parked run is told
        // which knob is theirs rather than being sent to check the network.
        assert!(
            err.to_string()
                .contains(crate::FailureKind::Timeout.remedy())
        );
        // And none of this may cost the run its failover: `Unreachable` is what
        // moves it to the next candidate and, failing that, parks it.
        assert_eq!(
            err.unavailable_reason(),
            Some(crate::UnavailableReason::Unreachable)
        );
    }

    /// The other half: a provider that *was* reached gets the patient circuit
    /// threshold, and one that was not does not. Asserted here, on an error the
    /// real path produced, because the runtime's own test for the two speeds
    /// writes the label by hand - which is exactly why the feature could be
    /// dead in production with that test passing.
    #[tokio::test]
    async fn a_timeout_from_the_inference_path_counts_as_having_reached_the_provider() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
        let addr = listener.local_addr().expect("has an address");
        std::thread::spawn(move || {
            let _held = listener.accept();
            std::thread::sleep(std::time::Duration::from_secs(30));
        });

        let client = crate::provider::build_http_client(Some(1)).expect("a client builds");
        let err = super::send_chat_request(
            &client,
            "test",
            &format!("http://{addr}/v1/chat/completions"),
            &[],
            &serde_json::json!({ "model": "m" }),
            None,
            None,
        )
        .await
        .expect_err("nothing ever answers");

        let kind = err.failure_kind().expect("classified");
        assert!(kind.provider_was_reached());
    }

    /// Asking to stream means asking for usage too, and the two travel
    /// together so that no provider can set one and forget the other.
    ///
    /// OpenRouter did forget, which was harmless only while nothing streamed:
    /// its choice-less usage chunk is where the price the account was charged
    /// arrives, so a streamed run would have reported every turn as free.
    #[test]
    fn asking_to_stream_asks_for_the_usage_as_well() {
        let mut body = serde_json::json!({ "model": "m" });
        super::make_streaming(&mut body);
        assert_eq!(body["stream"], serde_json::json!(true));
        assert_eq!(
            body["stream_options"],
            serde_json::json!({ "include_usage": true })
        );
    }

    /// The price a provider states survives a streamed call.
    ///
    /// The usage arrives in a chunk with no `choices`, and that arm was the one
    /// that built its `TokenUsage` without the cost passthrough its sibling
    /// had - so the one shape that carries a real invoice figure was the one
    /// that dropped it.
    #[test]
    fn a_usage_only_chunk_keeps_the_cost_the_provider_reported() {
        let mut buf = format!(
            "data: {}\n\n",
            serde_json::json!({
                "usage": {
                    "prompt_tokens": 50,
                    "completion_tokens": 25,
                    "cost": 0.00123
                }
            })
        );
        let chunk = super::parse_openai_sse_event(&mut buf)
            .unwrap()
            .unwrap()
            .unwrap();
        let tokens = chunk.tokens.expect("a usage chunk carries usage");
        assert_eq!(tokens.reported_cost_usd, Some(0.00123));
    }

    /// A streamed tool call carries the signature the model issued for it.
    ///
    /// Gemini 3.x refuses a function call replayed without its
    /// `thought_signature`, and `ToolCallDelta` had nowhere to put one - so a
    /// streamed tool call would have been rejected on the following turn, with
    /// an error naming the field rather than the path that lost it.
    #[test]
    fn a_streamed_tool_call_carries_its_thought_signature() {
        let mut buf = format!(
            "data: {}\n\n",
            serde_json::json!({
                "choices": [{
                    "delta": { "tool_calls": [{
                        "index": 0,
                        "id": "call-1",
                        "function": { "name": "read_file", "arguments": "{}" },
                        "extra_content": { "google": { "thought_signature": "sig-abc" } }
                    }]}
                }]
            })
        );
        let chunk = super::parse_openai_sse_event(&mut buf)
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            chunk.tool_calls[0].thought_signature.as_deref(),
            Some("sig-abc")
        );
    }

    /// The refusal that killed a run, and the shapes that must NOT trip it.
    ///
    /// A false positive here silently drops the temperature the caller asked
    /// for, which is worse than the error it is trying to avoid: the run keeps
    /// going and quietly samples differently from what the blueprint said.
    #[test]
    fn a_temperature_refusal_is_told_apart_from_other_errors() {
        assert!(super::temperature_refused(
            "Unsupported value: 'temperature' does not support 0.7 with this \
             model. Only the default (1) value is supported."
        ));
        // Case is not guaranteed by the API.
        assert!(super::temperature_refused(
            "UNSUPPORTED VALUE: 'TEMPERATURE' DOES NOT SUPPORT 0.7"
        ));
        // A different unsupported field is not ours to fix.
        assert!(!super::temperature_refused(
            "Unsupported value: 'reasoning_effort' does not support 'none' with this model."
        ));
        // Mentioning temperature is not the same as refusing over it.
        assert!(!super::temperature_refused(
            "temperature must be between 0 and 2"
        ));
        assert!(!super::temperature_refused("rate limited"));
    }

    use super::*;
    use crate::provider::{InferenceRequest, Message, SystemBlock, Tool};

    fn sample_request() -> InferenceRequest {
        InferenceRequest {
            system: vec![],
            messages: vec![
                Message {
                    role: "system".into(),
                    content: "You are helpful".into(),
                    cache_breakpoint: false,
                },
                Message {
                    role: "user".into(),
                    content: "Hello".into(),
                    cache_breakpoint: false,
                },
            ],
            model: "gpt-4".into(),
            max_tokens: 1024,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::json!({}),
            request_timeout_secs: None,
        }
    }

    // ─── ensure_user_turn ───────────────────────────────────────────────────

    /// A request with a system prompt and nothing else, which is exactly the
    /// first inference of any run: the task lives in a pinned region, so there
    /// is no user message until the assistant has said something to answer.
    #[test]
    fn a_conversation_with_no_messages_still_carries_a_user_turn() {
        let request = InferenceRequest {
            system: vec![crate::provider::SystemBlock {
                text: "## task\ncount the ERROR lines".to_string(),
                cache_hint: leviath_core::CacheHint::Always,
                volatility: leviath_core::Volatility::default(),
                region: String::new(),
            }],
            messages: vec![],
            model: "qwen3.8".to_string(),
            max_tokens: 64,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let messages = openai_messages(&request);
        let roles: Vec<&str> = messages.iter().filter_map(|m| m["role"].as_str()).collect();
        assert_eq!(roles, vec!["system", "user"]);
        // The stand-in points at where the real task is, so it cannot read as
        // a fresh request from the person.
        assert_eq!(
            messages[1]["content"],
            serde_json::json!("Proceed with the task described in the system instructions.")
        );
    }

    /// The window holding nothing but the assistant's own prose. No tool call,
    /// so `satisfy_call_turn_order` does not fire and this is the shape that
    /// reached Ollama without a user turn (issue #469).
    #[test]
    fn a_conversation_of_only_assistant_prose_gains_a_user_turn() {
        let request = InferenceRequest {
            system: vec![crate::provider::SystemBlock {
                text: "instructions".to_string(),
                cache_hint: leviath_core::CacheHint::Always,
                volatility: leviath_core::Volatility::default(),
                region: String::new(),
            }],
            messages: vec![crate::provider::Message {
                role: "assistant".to_string(),
                content: crate::provider::MessageContent::Text("still working".to_string()),
                cache_breakpoint: false,
            }],
            model: "qwen3.8".to_string(),
            max_tokens: 64,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let messages = openai_messages(&request);
        let roles: Vec<&str> = messages.iter().filter_map(|m| m["role"].as_str()).collect();
        // Ahead of the assistant, where an opening request belongs, rather than
        // after its last word where it would read as a new instruction.
        assert_eq!(roles, vec!["system", "user", "assistant"]);
    }

    /// Tool responses whose calls have aged out are dropped, which can empty a
    /// conversation that looked non-empty on the way in.
    #[test]
    fn a_conversation_emptied_by_dropping_orphans_gains_a_user_turn() {
        let request = InferenceRequest {
            system: vec![crate::provider::SystemBlock {
                text: "instructions".to_string(),
                cache_hint: leviath_core::CacheHint::Always,
                volatility: leviath_core::Volatility::default(),
                region: String::new(),
            }],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: crate::provider::MessageContent::Blocks(vec![
                    crate::provider::ContentBlock::ToolResult {
                        tool_use_id: "gone".to_string(),
                        content: "a.log".to_string(),
                        is_error: false,
                    },
                ]),
                cache_breakpoint: false,
            }],
            model: "qwen3.8".to_string(),
            max_tokens: 64,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let messages = openai_messages(&request);
        let roles: Vec<&str> = messages.iter().filter_map(|m| m["role"].as_str()).collect();
        assert_eq!(roles, vec!["system", "user"]);
    }

    /// The histories `satisfy_call_turn_order` already covers must not gain a
    /// second stand-in: one user turn is the requirement, not one per request.
    #[test]
    fn a_conversation_that_already_has_a_user_turn_is_left_alone() {
        let call = crate::provider::Message {
            role: "assistant".to_string(),
            content: crate::provider::MessageContent::Blocks(vec![
                crate::provider::ContentBlock::ToolUse {
                    id: "c1".to_string(),
                    name: "list_dir".to_string(),
                    input: serde_json::json!({"path": "logs/"}),
                    thought_signature: None,
                },
            ]),
            cache_breakpoint: false,
        };
        let result = crate::provider::Message {
            role: "user".to_string(),
            content: crate::provider::MessageContent::Blocks(vec![
                crate::provider::ContentBlock::ToolResult {
                    tool_use_id: "c1".to_string(),
                    content: "a.log".to_string(),
                    is_error: false,
                },
            ]),
            cache_breakpoint: false,
        };
        let request = InferenceRequest {
            system: vec![crate::provider::SystemBlock {
                text: "instructions".to_string(),
                cache_hint: leviath_core::CacheHint::Always,
                volatility: leviath_core::Volatility::default(),
                region: String::new(),
            }],
            messages: vec![call.clone(), result.clone(), call, result],
            model: "qwen3.8".to_string(),
            max_tokens: 64,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let messages = openai_messages(&request);
        let users = messages.iter().filter(|m| m["role"] == "user").count();
        assert_eq!(users, 1, "the turn-order pass already supplied one");
    }

    /// With no system message there is nothing to insert after, and index 0 has
    /// to stay in range.
    #[test]
    fn a_conversation_with_no_system_message_gains_a_leading_user_turn() {
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "assistant".to_string(),
                content: crate::provider::MessageContent::Text("still working".to_string()),
                cache_breakpoint: false,
            }],
            model: "qwen3.8".to_string(),
            max_tokens: 64,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let messages = openai_messages(&request);
        let roles: Vec<&str> = messages.iter().filter_map(|m| m["role"].as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant"]);
    }

    /// The shape issue #469 dies on, and the one the other two passes miss.
    ///
    /// A sliding window that has evicted a call turn but kept its response
    /// leaves that response stranded at the head. It is normally dropped as an
    /// orphan - except against Ollama, which restarts tool-call ids at
    /// `ollama_0` every turn (#470), so a *later* turn's call puts that id in
    /// `called` and the stranded response looks answered.
    ///
    /// The conversation then opens on a `tool` message, so when the first real
    /// call turn arrives [`satisfy_call_turn_order`] sees `prev_role == "tool"`,
    /// considers the ordering satisfied, and inserts nothing. Every remaining
    /// role is assistant or tool, and the request reaches Qwen with no user
    /// query at all.
    #[test]
    fn a_stranded_tool_response_at_the_head_does_not_suppress_the_user_turn() {
        let stranded = crate::provider::Message {
            role: "user".to_string(),
            content: crate::provider::MessageContent::Blocks(vec![
                crate::provider::ContentBlock::ToolResult {
                    tool_use_id: "ollama_0".to_string(),
                    content: "logs/a.log".to_string(),
                    is_error: false,
                },
            ]),
            cache_breakpoint: false,
        };
        let call = crate::provider::Message {
            role: "assistant".to_string(),
            content: crate::provider::MessageContent::Blocks(vec![
                crate::provider::ContentBlock::ToolUse {
                    // The same id the evicted turn used, which is exactly what
                    // Ollama sends and why the stranded response survives.
                    id: "ollama_0".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": "logs/a.log"}),
                    thought_signature: None,
                },
            ]),
            cache_breakpoint: false,
        };
        let answer = crate::provider::Message {
            role: "user".to_string(),
            content: crate::provider::MessageContent::Blocks(vec![
                crate::provider::ContentBlock::ToolResult {
                    tool_use_id: "ollama_0".to_string(),
                    content: "ERROR disk full".to_string(),
                    is_error: false,
                },
            ]),
            cache_breakpoint: false,
        };
        let request = InferenceRequest {
            system: vec![crate::provider::SystemBlock {
                text: "## task\ncount the ERROR lines".to_string(),
                cache_hint: leviath_core::CacheHint::Always,
                volatility: leviath_core::Volatility::default(),
                region: String::new(),
            }],
            messages: vec![stranded, call, answer],
            model: "qwen3.8-32k".to_string(),
            max_tokens: 64,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        // What the two earlier passes leave behind on their own: the stranded
        // response is kept, so the call turn reads as correctly ordered and no
        // user turn is added anywhere.
        let mut raw = vec![serde_json::json!({
            "role": "system",
            "content": request.system[0].text,
        })];
        for msg in &request.messages {
            raw.extend(message_to_openai_with(
                &msg.role,
                &msg.content,
                ToolArgsFormat::Object,
            ));
        }
        let without = satisfy_call_turn_order(drop_unpaired_tool_turns(raw));
        let roles: Vec<&str> = without.iter().filter_map(|m| m["role"].as_str()).collect();
        assert_eq!(
            roles,
            vec!["system", "tool", "assistant", "tool"],
            "the stranded response survives and suppresses the inserted turn"
        );

        let messages = openai_messages_with(&request, ToolArgsFormat::Object);
        let roles: Vec<&str> = messages.iter().filter_map(|m| m["role"].as_str()).collect();
        assert_eq!(roles, vec!["system", "user", "tool", "assistant", "tool"]);
    }

    // ─── merge_extra_params ─────────────────────────────────────────────────

    #[test]
    fn merge_extra_params_adds_object_keys_without_overwriting() {
        let mut target = serde_json::Map::new();
        target.insert("temperature".to_string(), serde_json::json!(0.5));
        merge_extra_params(
            &mut target,
            &serde_json::json!({ "top_p": 0.9, "temperature": 0.1, "seed": 7 }),
        );
        // New keys added …
        assert_eq!(target["top_p"], serde_json::json!(0.9));
        assert_eq!(target["seed"], serde_json::json!(7));
        // … but an existing key (an explicit request field) is not overwritten.
        assert_eq!(target["temperature"], serde_json::json!(0.5));
    }

    #[test]
    fn merge_extra_params_ignores_non_object_extra() {
        let mut target = serde_json::Map::new();
        target.insert("a".to_string(), serde_json::json!(1));
        merge_extra_params(&mut target, &serde_json::Value::Null);
        merge_extra_params(&mut target, &serde_json::json!("string"));
        assert_eq!(target.len(), 1);
    }

    #[test]
    fn build_openai_request_body_passes_through_extra_params() {
        let mut req = sample_request();
        req.extra = serde_json::json!({ "top_p": 0.8, "stop": ["END"] });
        let body = build_openai_request_body(&req);
        assert_eq!(body["top_p"], serde_json::json!(0.8));
        assert_eq!(body["stop"], serde_json::json!(["END"]));
    }

    // ─── build_openai_request_body ──────────────────────────────────────────

    #[test]
    fn build_request_body_basic() {
        let req = sample_request();
        let body = build_openai_request_body(&req);
        assert_eq!(body["model"], "gpt-4");
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["temperature"], 0.5);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["content"], "Hello");
    }

    #[test]
    fn build_request_body_no_tools_omits_tools_key() {
        let req = sample_request();
        let body = build_openai_request_body(&req);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn build_request_body_with_tools() {
        let mut req = sample_request();
        req.tools = vec![Tool {
            name: "search".into(),
            description: "Search the web".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }];
        let body = build_openai_request_body(&req);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "search");
        assert_eq!(tools[0]["function"]["description"], "Search the web");
    }

    #[test]
    fn build_request_body_multiple_tools() {
        let mut req = sample_request();
        req.tools = vec![
            Tool {
                name: "tool_a".into(),
                description: "A".into(),
                parameters: serde_json::json!({}),
            },
            Tool {
                name: "tool_b".into(),
                description: "B".into(),
                parameters: serde_json::json!({}),
            },
        ];
        let body = build_openai_request_body(&req);
        assert_eq!(body["tools"].as_array().unwrap().len(), 2);
    }

    // ─── parse_openai_response ──────────────────────────────────────────────

    #[test]
    fn parse_response_basic() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "Hello there!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5
            }
        });
        let resp = parse_openai_response(&body).unwrap();
        assert_eq!(resp.content, "Hello there!");
        assert!(resp.tool_calls.is_empty());
        assert_eq!(resp.tokens_used.prompt_tokens, 10);
        assert_eq!(resp.tokens_used.completion_tokens, 5);
        assert_eq!(resp.tokens_used.total_tokens, 15);
        assert_eq!(resp.finish_reason, crate::provider::FinishReason::Complete);
    }

    #[test]
    fn parse_response_no_choices_returns_error() {
        let body = serde_json::json!({});
        let err = parse_openai_response(&body).unwrap_err();
        assert!(err.to_string().contains("No choices"));
    }

    // ─── reasoning-only responses ──────────────────────────────────────────

    #[test]
    fn reasoning_fills_in_for_a_null_content() {
        // What a reasoning model on OpenRouter actually sends: `content: null`
        // and the answer under `reasoning`. Reading `content` alone handed the
        // runtime an empty response, so the agent was nudged, looped, and the
        // run finished having said nothing.
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": serde_json::Value::Null,
                    "reasoning": "2 + 2 is 4.",
                },
                "finish_reason": "length"
            }]
        });
        let resp = parse_openai_response(&body).unwrap();
        assert_eq!(resp.content, "2 + 2 is 4.");
    }

    #[test]
    fn reasoning_details_are_joined_when_the_flat_field_is_absent() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "",
                    "reasoning_details": [
                        { "type": "reasoning.text", "text": "first " },
                        { "type": "reasoning.text", "text": "second" },
                        { "type": "reasoning.encrypted" },
                    ],
                }
            }]
        });
        let resp = parse_openai_response(&body).unwrap();
        assert_eq!(resp.content, "first second");
    }

    #[test]
    fn reasoning_never_displaces_real_content_or_a_tool_call() {
        // Reasoning is working-out, not output. A message that has either of
        // the two things the runtime acts on is not empty, and appending the
        // model's scratchpad to it would put the scratchpad in the transcript.
        let with_content = serde_json::json!({
            "choices": [{
                "message": { "content": "the answer", "reasoning": "scratchpad" }
            }]
        });
        assert_eq!(
            parse_openai_response(&with_content).unwrap().content,
            "the answer"
        );

        let with_tool_call = serde_json::json!({
            "choices": [{
                "message": {
                    "content": serde_json::Value::Null,
                    "reasoning": "scratchpad",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": { "name": "read_file", "arguments": "{}" }
                    }]
                }
            }]
        });
        let resp = parse_openai_response(&with_tool_call).unwrap();
        assert_eq!(resp.content, "");
        assert_eq!(resp.tool_calls.len(), 1);
    }

    #[test]
    fn blank_reasoning_is_not_treated_as_content() {
        let body = serde_json::json!({
            "choices": [{
                "message": { "content": serde_json::Value::Null, "reasoning": "   " }
            }]
        });
        assert_eq!(parse_openai_response(&body).unwrap().content, "");

        let empty_details = serde_json::json!({
            "choices": [{
                "message": {
                    "content": serde_json::Value::Null,
                    "reasoning_details": [{ "type": "reasoning.encrypted" }]
                }
            }]
        });
        assert_eq!(parse_openai_response(&empty_details).unwrap().content, "");

        // A `reasoning_details` that is not an array at all. Nothing sends this
        // today; the point is that a shape change upstream degrades to an empty
        // answer rather than a panic in the middle of a run.
        let not_an_array = serde_json::json!({
            "choices": [{
                "message": {
                    "content": serde_json::Value::Null,
                    "reasoning_details": "unexpected"
                }
            }]
        });
        assert_eq!(parse_openai_response(&not_an_array).unwrap().content, "");
    }

    // ─── error envelopes delivered with a success status ───────────────────

    #[test]
    fn an_error_envelope_beats_the_missing_choices_message() {
        // OpenRouter answers 200 with this shape when an upstream provider
        // rejects a request it had already accepted. "No choices in response"
        // threw away the only text that said why.
        let body = serde_json::json!({
            "error": { "code": 400, "message": "nonexistent/model-xyz is not a valid model ID" }
        });
        let err = parse_openai_response(&body).unwrap_err();
        assert!(err.to_string().contains("not a valid model ID"), "{err}");
        assert!(err.to_string().contains("400"), "{err}");
        // A bad model id is this request's problem, not the provider's, so it
        // must not fail over or count against the circuit breaker.
        assert!(err.unavailable_reason().is_none(), "{err}");
    }

    #[test]
    fn a_402_envelope_still_fails_over() {
        // The whole point of reading the envelope: a drained account reported
        // this way has to reach the same failover path a 402 status does.
        let body = serde_json::json!({
            "error": { "code": 402, "message": "Insufficient credits" }
        });
        let err = parse_openai_response(&body).unwrap_err();
        assert_eq!(
            err.unavailable_reason(),
            Some(crate::provider::UnavailableReason::CreditsExhausted)
        );
    }

    #[test]
    fn an_envelope_without_a_code_falls_back_to_its_message() {
        let body = serde_json::json!({ "error": { "message": "upstream timed out" } });
        let err = parse_openai_response(&body).unwrap_err();
        assert_eq!(err.to_string(), "API error: upstream timed out");

        // A string code is read too - not every gateway sends a number.
        let stringly = serde_json::json!({
            "error": { "code": "401", "message": "no auth" }
        });
        assert_eq!(
            parse_openai_response(&stringly)
                .unwrap_err()
                .unavailable_reason(),
            Some(crate::provider::UnavailableReason::AuthFailed)
        );
    }

    #[test]
    fn an_envelope_with_no_readable_fields_still_reports_something() {
        let body = serde_json::json!({ "error": { "kind": "weird" } });
        let err = parse_openai_response(&body).unwrap_err();
        assert!(err.to_string().contains("weird"), "{err}");

        let coded_only = serde_json::json!({ "error": { "code": 503 } });
        let err = parse_openai_response(&coded_only).unwrap_err();
        assert!(err.to_string().contains("503"), "{err}");
    }

    #[test]
    fn a_null_error_field_is_not_an_error() {
        // Every successful OpenAI-compatible response from some gateways
        // carries `"error": null`; treating that as a failure would reject
        // every good response.
        let body = serde_json::json!({
            "error": serde_json::Value::Null,
            "choices": [{ "message": { "content": "fine" } }]
        });
        assert_eq!(parse_openai_response(&body).unwrap().content, "fine");
    }

    #[test]
    fn parse_response_no_message_returns_error() {
        let body = serde_json::json!({
            "choices": [{}]
        });
        let err = parse_openai_response(&body).unwrap_err();
        assert!(err.to_string().contains("No message"));
    }

    #[test]
    fn parse_response_with_tool_calls() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"city\":\"NYC\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 20, "completion_tokens": 10}
        });
        let resp = parse_openai_response(&body).unwrap();
        assert_eq!(resp.content, "");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "call_1");
        assert_eq!(resp.tool_calls[0].name, "get_weather");
        assert_eq!(resp.tool_calls[0].arguments["city"], "NYC");
        assert_eq!(resp.finish_reason, crate::provider::FinishReason::ToolCall);
    }

    #[test]
    fn parse_response_cached_tokens() {
        let body = serde_json::json!({
            "choices": [{
                "message": {"content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 10,
                "prompt_tokens_details": {
                    "cached_tokens": 80,
                    // Reported by gateways fronting a provider that charges a
                    // write premium. It used to be dropped, so a run paying the
                    // 1.25x rate recorded none of it.
                    "cache_write_tokens": 15
                }
            }
        });
        let resp = parse_openai_response(&body).unwrap();
        assert_eq!(resp.tokens_used.cached_tokens, 80);
        assert_eq!(resp.tokens_used.cache_write_tokens, 15);
    }

    #[test]
    fn parse_response_missing_usage_defaults_to_zero() {
        let body = serde_json::json!({
            "choices": [{
                "message": {"content": "hi"},
                "finish_reason": "stop"
            }]
        });
        let resp = parse_openai_response(&body).unwrap();
        assert_eq!(resp.tokens_used.prompt_tokens, 0);
        assert_eq!(resp.tokens_used.completion_tokens, 0);
    }

    #[test]
    fn parse_response_finish_reason_length() {
        let body = serde_json::json!({
            "choices": [{
                "message": {"content": "truncated"},
                "finish_reason": "length"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 5}
        });
        let resp = parse_openai_response(&body).unwrap();
        assert_eq!(
            resp.finish_reason,
            crate::provider::FinishReason::TokenLimit
        );
    }

    // ─── parse_openai_sse_event ─────────────────────────────────────────────

    #[test]
    fn sse_event_incomplete_returns_none() {
        let mut buf = "data: {\"choices\":[".to_string();
        assert!(parse_openai_sse_event(&mut buf).is_none());
    }

    #[test]
    fn sse_event_done_returns_stream_end() {
        let mut buf = "data: [DONE]\n\n".to_string();
        let result = parse_openai_sse_event(&mut buf);
        assert!(result.is_some() && result.unwrap().is_none());
        assert!(buf.is_empty());
    }

    #[test]
    fn sse_event_content_delta() {
        let mut buf = format!(
            "data: {}\n\n",
            serde_json::json!({
                "choices": [{
                    "delta": {"content": "Hello"},
                    "finish_reason": null
                }]
            })
        );
        let result = parse_openai_sse_event(&mut buf);
        let chunk = result.unwrap().unwrap().unwrap();
        assert_eq!(chunk.delta, "Hello");
        assert!(chunk.finish_reason.is_none());
        assert!(chunk.tool_calls.is_empty());
    }

    #[test]
    fn sse_event_with_finish_reason() {
        let mut buf = format!(
            "data: {}\n\n",
            serde_json::json!({
                "choices": [{
                    "delta": {"content": ""},
                    "finish_reason": "stop"
                }]
            })
        );
        let chunk = parse_openai_sse_event(&mut buf).unwrap().unwrap().unwrap();
        assert_eq!(
            chunk.finish_reason,
            Some(crate::provider::FinishReason::Complete)
        );
    }

    #[test]
    fn sse_event_tool_call_delta() {
        let mut buf = format!(
            "data: {}\n\n",
            serde_json::json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_abc",
                            "function": {
                                "name": "search",
                                "arguments": "{\"q\":"
                            }
                        }]
                    }
                }]
            })
        );
        let chunk = parse_openai_sse_event(&mut buf).unwrap().unwrap().unwrap();
        assert_eq!(chunk.tool_calls.len(), 1);
        assert_eq!(chunk.tool_calls[0].index, 0);
        assert_eq!(chunk.tool_calls[0].id.as_deref(), Some("call_abc"));
        assert_eq!(chunk.tool_calls[0].name.as_deref(), Some("search"));
        assert_eq!(chunk.tool_calls[0].arguments_delta, "{\"q\":");
    }

    /// A gateway that reports what the call cost has that figure taken
    /// verbatim. OpenRouter does this when usage accounting is on, and it is
    /// preferred over anything computed from a rate card.
    #[test]
    fn a_reported_cost_is_carried_through_from_the_buffered_response() {
        let body = serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "hi"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "cost": 0.00123}
        });
        let r = parse_openai_response(&body).unwrap();
        assert_eq!(r.tokens_used.reported_cost_usd, Some(0.00123));
    }

    /// The same for a streamed call, or streaming and buffering the identical
    /// request would report different money.
    #[test]
    fn a_reported_cost_is_carried_through_from_a_stream_chunk() {
        let mut buf = format!(
            "data: {}\n\n",
            serde_json::json!({
                "choices": [{"delta": {"content": "x"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 8, "completion_tokens": 2, "cost": 0.0004}
            })
        );
        let chunk = parse_openai_sse_event(&mut buf).unwrap().unwrap().unwrap();
        assert_eq!(chunk.tokens.unwrap().reported_cost_usd, Some(0.0004));
    }

    #[test]
    fn sse_event_usage_only_chunk() {
        let mut buf = format!(
            "data: {}\n\n",
            serde_json::json!({
                "usage": {
                    "prompt_tokens": 50,
                    "completion_tokens": 25,
                    "prompt_tokens_details": {"cached_tokens": 10, "cache_write_tokens": 4}
                }
            })
        );
        let chunk = parse_openai_sse_event(&mut buf).unwrap().unwrap().unwrap();
        assert_eq!(chunk.delta, "");
        let tokens = chunk.tokens.unwrap();
        // 50 reported inclusive of its details: 10 cache reads and 4 writes,
        // leaving 36 fresh. The streaming path normalises the same way the
        // non-streaming one does, or a streamed call and a buffered call of the
        // same request would cost different amounts.
        assert_eq!(tokens.prompt_tokens, 36, "fresh input only");
        assert_eq!(tokens.completion_tokens, 25);
        assert_eq!(tokens.cached_tokens, 10);
        assert_eq!(tokens.cache_write_tokens, 4);
        assert_eq!(tokens.input_tokens(), 50);
        assert_eq!(tokens.total_tokens, 75);
    }

    #[test]
    fn sse_event_multiple_events_in_buffer() {
        let mut buf = format!(
            "data: {}\n\ndata: {}\n\n",
            serde_json::json!({"choices": [{"delta": {"content": "A"}}]}),
            serde_json::json!({"choices": [{"delta": {"content": "B"}}]})
        );
        let chunk1 = parse_openai_sse_event(&mut buf).unwrap().unwrap().unwrap();
        assert_eq!(chunk1.delta, "A");

        let chunk2 = parse_openai_sse_event(&mut buf).unwrap().unwrap().unwrap();
        assert_eq!(chunk2.delta, "B");
    }

    #[test]
    fn sse_event_with_usage_in_choice_chunk() {
        let mut buf = format!(
            "data: {}\n\n",
            serde_json::json!({
                "choices": [{"delta": {"content": "X"}, "finish_reason": "stop"}],
                "usage": {
                    "prompt_tokens": 100,
                    "completion_tokens": 50,
                    "prompt_tokens_details": {"cached_tokens": 30, "cache_write_tokens": 7}
                }
            })
        );
        let chunk = parse_openai_sse_event(&mut buf).unwrap().unwrap().unwrap();
        assert_eq!(chunk.delta, "X");
        let tokens = chunk.tokens.unwrap();
        // 100 inclusive of 30 cached and 7 written leaves 63 fresh.
        assert_eq!(tokens.prompt_tokens, 63, "fresh input only");
        assert_eq!(tokens.cached_tokens, 30);
        assert_eq!(tokens.cache_write_tokens, 7);
        assert_eq!(tokens.input_tokens(), 100);
        assert_eq!(tokens.reported_cost_usd, None, "this gateway reported none");
    }

    #[test]
    fn sse_event_invalid_json_skipped() {
        let mut buf = "data: not-json\n\n".to_string();
        // Invalid JSON line is skipped; no valid data line follows → None
        assert!(parse_openai_sse_event(&mut buf).is_none());
    }

    // ── Additional coverage tests ──────────────────────────────────────────

    /// A request carrying nothing at all still goes out with a user turn.
    ///
    /// Every chat API rejects an empty message list, so there is no shape this
    /// makes worse, and guaranteeing the turn unconditionally keeps one rule
    /// rather than one rule and an exception nobody would think to test.
    #[test]
    fn build_request_body_empty_messages() {
        let req = InferenceRequest {
            system: vec![],
            messages: vec![],
            model: "gpt-4".into(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::json!({}),
            request_timeout_secs: None,
        };
        let body = build_openai_request_body(&req);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn parse_response_multiple_tool_calls() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "function": { "name": "search", "arguments": "{}" }
                        },
                        {
                            "id": "call_2",
                            "function": { "name": "write", "arguments": "{\"file\":\"a.txt\"}" }
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        let resp = parse_openai_response(&body).unwrap();
        assert_eq!(resp.tool_calls.len(), 2);
        assert_eq!(resp.tool_calls[0].name, "search");
        assert_eq!(resp.tool_calls[1].name, "write");
        assert_eq!(resp.tool_calls[1].arguments["file"], "a.txt");
    }

    #[test]
    fn parse_response_malformed_tool_arguments_defaults_to_empty_object() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": { "name": "test", "arguments": "not-valid-json" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 5}
        });
        let resp = parse_openai_response(&body).unwrap();
        assert_eq!(resp.tool_calls.len(), 1);
        assert!(resp.tool_calls[0].arguments.is_object());
    }

    #[test]
    fn sse_event_empty_buffer() {
        let mut buf = String::new();
        assert!(parse_openai_sse_event(&mut buf).is_none());
    }

    #[test]
    fn sse_event_consumes_only_first_event() {
        let mut buf = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({"choices": [{"delta": {"content": "X"}}]})
        );
        let chunk = parse_openai_sse_event(&mut buf).unwrap().unwrap().unwrap();
        assert_eq!(chunk.delta, "X");
        // Buffer should still have the [DONE] event
        assert!(buf.contains("[DONE]"));
    }

    #[test]
    fn sse_event_no_data_prefix_skipped() {
        // Event with no "data: " prefix line
        let mut buf = "event: something\n\n".to_string();
        assert!(parse_openai_sse_event(&mut buf).is_none());
    }

    #[test]
    fn sse_event_tool_call_delta_no_id() {
        let mut buf = format!(
            "data: {}\n\n",
            serde_json::json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 1,
                            "function": {
                                "arguments": "\"val\"}"
                            }
                        }]
                    }
                }]
            })
        );
        let chunk = parse_openai_sse_event(&mut buf).unwrap().unwrap().unwrap();
        assert_eq!(chunk.tool_calls.len(), 1);
        assert_eq!(chunk.tool_calls[0].index, 1);
        assert!(chunk.tool_calls[0].id.is_none());
        assert!(chunk.tool_calls[0].name.is_none());
    }

    #[test]
    fn sse_event_error_chunk_surfaces_as_an_error() {
        // A stream is where OpenRouter reports a failure it only found after
        // committing to a 200 - the upstream provider going down, or the
        // account draining between chunks. It has no `choices` and no `usage`,
        // so it used to fall through to `continue` and simply end the stream:
        // a truncated answer with nothing anywhere saying why.
        let mut buf = format!(
            "data: {}\n\n",
            serde_json::json!({
                "error": { "code": 402, "message": "Insufficient credits" }
            })
        );
        let err = parse_openai_sse_event(&mut buf)
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(
            err.unavailable_reason(),
            Some(crate::provider::UnavailableReason::CreditsExhausted),
            "{err}"
        );
    }

    #[test]
    fn sse_event_null_error_field_is_ignored() {
        let mut buf = format!(
            "data: {}\n\n",
            serde_json::json!({
                "error": serde_json::Value::Null,
                "choices": [{ "delta": { "content": "hi" } }]
            })
        );
        let chunk = parse_openai_sse_event(&mut buf).unwrap().unwrap().unwrap();
        assert_eq!(chunk.delta, "hi");
    }

    #[test]
    fn sse_event_no_choices_no_usage_continues() {
        let mut buf = format!("data: {}\n\n", serde_json::json!({"id": "chatcmpl-123"}));
        // No choices and no usage → should continue, effectively None since
        // no data line produces a result
        let result = parse_openai_sse_event(&mut buf);
        assert!(result.is_none());
    }

    // ─── OpenAiSseStream (Stream-level, not just parse_openai_sse_event) ───

    struct StaticByteStream {
        data: Vec<Vec<u8>>,
        idx: usize,
    }

    impl futures_core::Stream for StaticByteStream {
        type Item = std::result::Result<bytes::Bytes, reqwest::Error>;
        fn poll_next(
            mut self: Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            if self.idx < self.data.len() {
                let chunk = bytes::Bytes::from(self.data[self.idx].clone());
                self.idx += 1;
                std::task::Poll::Ready(Some(Ok(chunk)))
            } else {
                std::task::Poll::Ready(None)
            }
        }
    }

    #[tokio::test]
    async fn openai_sse_stream_yields_content_delta() {
        use tokio_stream::StreamExt;
        let data = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n".to_vec();
        let stream = StaticByteStream {
            data: vec![data],
            idx: 0,
        };
        let mut sse = OpenAiSseStream::new(stream);
        let chunk = sse.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "hi");
    }

    #[tokio::test]
    async fn openai_sse_stream_done_marker_ends_stream() {
        use tokio_stream::StreamExt;
        let data = b"data: [DONE]\n\n".to_vec();
        let stream = StaticByteStream {
            data: vec![data],
            idx: 0,
        };
        let mut sse = OpenAiSseStream::new(stream);
        assert!(sse.next().await.is_none());
    }

    #[tokio::test]
    async fn openai_sse_stream_ends_with_incomplete_buffer_returns_none() {
        use tokio_stream::StreamExt;
        // No trailing "\n\n" - the event never completes.
        let data = b"data: {\"choices\":[{\"delta\":{}}]}".to_vec();
        let stream = StaticByteStream {
            data: vec![data],
            idx: 0,
        };
        let mut sse = OpenAiSseStream::new(stream);
        assert!(sse.next().await.is_none());
    }

    #[tokio::test]
    async fn openai_sse_stream_multiple_chunks_across_reads() {
        use tokio_stream::StreamExt;
        // First read has no complete event; second read completes it.
        let stream = StaticByteStream {
            data: vec![
                b"data: {\"choices\":".to_vec(),
                b"[{\"delta\":{\"content\":\"ok\"}}]}\n\n".to_vec(),
            ],
            idx: 0,
        };
        let mut sse = OpenAiSseStream::new(stream);
        let chunk = sse.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "ok");
    }

    #[tokio::test]
    async fn openai_sse_stream_flushes_trailing_buffered_event_when_stream_ends() {
        use tokio_stream::StreamExt;
        // A comment-only SSE event (no `data:` line) glued directly to a real
        // data event in the SAME chunk. `parse_openai_sse_event` consumes the
        // comment event from the buffer but returns plain `None` (its
        // for-loop finds no `data:` line to act on) - indistinguishable to
        // the caller from "incomplete". So `poll_next`'s top-of-loop check
        // falls through and polls the inner stream again, which then reports
        // end-of-stream; only *there* does poll_next's own end-of-stream
        // re-check find the still-buffered, still-unconsumed data event.
        let data = b": ping\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n".to_vec();
        let stream = StaticByteStream {
            data: vec![data],
            idx: 0,
        };
        let mut sse = OpenAiSseStream::new(stream);
        let chunk = sse.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "hi");
        assert!(sse.next().await.is_none());
    }

    #[tokio::test]
    async fn openai_sse_stream_skips_invalid_utf8_chunk_and_continues() {
        // covers the implicit else of `if let Ok(text) = from_utf8(&bytes)`
        use tokio_stream::StreamExt;
        let stream = StaticByteStream {
            data: vec![
                vec![0xFF, 0xFE, 0x00],
                b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n".to_vec(),
            ],
            idx: 0,
        };
        let mut sse = OpenAiSseStream::new(stream);
        let chunk = sse.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "ok");
    }

    // ─── MessageContent::Blocks code paths in build_openai_request_body ────

    #[test]
    fn message_to_openai_blocks_tool_use_with_text() {
        let content = MessageContent::Blocks(vec![
            ContentBlock::Text {
                text: "Let me call a tool.".into(),
            },
            ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "search".into(),
                input: serde_json::json!({"query": "rust"}),
                thought_signature: None,
            },
        ]);
        let messages = message_to_openai("assistant", &content);
        let messages = &messages;
        assert_eq!(messages.len(), 1);
        let msg = &messages[0];
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["content"], "Let me call a tool.");
        let tool_calls = msg["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_1");
        assert_eq!(tool_calls[0]["type"], "function");
        assert_eq!(tool_calls[0]["function"]["name"], "search");
        // arguments is serialized as a string
        let args: serde_json::Value =
            serde_json::from_str(tool_calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["query"], "rust");
    }

    #[test]
    fn message_to_openai_blocks_tool_use_without_text() {
        let content = MessageContent::Blocks(vec![ContentBlock::ToolUse {
            id: "call_2".into(),
            name: "write_file".into(),
            input: serde_json::json!({"path": "/tmp/a.txt"}),
            thought_signature: None,
        }]);
        let messages = message_to_openai("assistant", &content);
        // A call-only block yields one assistant turn and no `content` key.
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "assistant");
        assert!(messages[0].get("content").is_none());
        let tool_calls = messages[0]["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_2");
        assert_eq!(tool_calls[0]["function"]["name"], "write_file");
    }

    #[test]
    fn message_to_openai_blocks_tool_results_each_become_a_tool_turn() {
        let content = MessageContent::Blocks(vec![
            ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "result A".into(),
                is_error: false,
            },
            ContentBlock::ToolResult {
                tool_use_id: "call_2".into(),
                content: "result B".into(),
                is_error: false,
            },
        ]);
        let messages = message_to_openai("user", &content);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "tool");
        assert_eq!(messages[0]["tool_call_id"], "call_1");
        assert_eq!(messages[0]["content"], "result A");
        assert_eq!(messages[1]["tool_call_id"], "call_2");
        assert_eq!(messages[1]["content"], "result B");
    }

    /// The shape Gemini rejects with HTTP 400: a call turn whose response has
    /// aged out of the context window (or vice versa) must not be sent.
    /// The shape that killed `wide-researcher-x-1787453815`: a `challenge`
    /// stage ran grok on OpenRouter and called tools, then `polish` handed the
    /// same conversation to Gemini, which refused function calls it never
    /// signed ("Function call is missing a thought_signature in functionCall
    /// parts ... `default_api:read_file`, position 9").
    ///
    /// Nothing had dropped a signature. Grok never issues one, and the
    /// conversation region carries turns across a stage boundary, so any
    /// blueprint that changes model family mid-run arrives here.
    #[test]
    fn a_call_made_by_another_model_is_folded_into_text_for_gemini() {
        let request = InferenceRequest {
            system: vec![],
            messages: vec![
                Message {
                    role: "assistant".to_string(),
                    content: MessageContent::Blocks(vec![
                        ContentBlock::Text {
                            text: "Checking the source.".to_string(),
                        },
                        ContentBlock::ToolUse {
                            id: "call_grok_1".to_string(),
                            name: "read_file".to_string(),
                            input: serde_json::json!({ "path": "README.md" }),
                            // Grok issues no signature. This is the whole bug.
                            thought_signature: None,
                        },
                    ]),
                    cache_breakpoint: false,
                },
                Message {
                    role: "user".to_string(),
                    content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                        tool_use_id: "call_grok_1".to_string(),
                        content: "# Leviath\nAn agent framework.".to_string(),
                        is_error: false,
                    }]),
                    cache_breakpoint: false,
                },
            ],
            model: "google/gemini-3.1-pro-preview".to_string(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let msgs = openai_messages(&request);
        let wire = serde_json::to_string(&msgs).expect("serialises");

        // The claim the API makes: no functionCall part may be unsigned. The
        // simplest way to satisfy it for a call this model did not make is to
        // stop calling it a call.
        assert!(
            !wire.contains("tool_calls"),
            "an unsigned call must not be replayed as a call: {wire}"
        );
        assert!(
            !wire.contains("\"role\":\"tool\""),
            "and its result must not be left stranded as a tool turn: {wire}"
        );

        // What the run learned still has to reach the model, or the polish
        // stage rewrites a report it can no longer see the evidence for.
        assert!(wire.contains("read_file"), "the call is still described");
        assert!(
            wire.contains("An agent framework."),
            "and so is what it returned: {wire}"
        );
        assert!(wire.contains("Checking the source."), "original text kept");
    }

    /// The signed call is the one Gemini itself made, so it must survive as a
    /// call. Folding everything would throw away the turn structure the model
    /// depends on when it is talking to itself.
    #[test]
    fn a_signed_call_is_left_alone_and_an_unsigned_neighbour_is_not() {
        let request = InferenceRequest {
            system: vec![],
            messages: vec![Message {
                role: "assistant".to_string(),
                content: MessageContent::Blocks(vec![
                    ContentBlock::ToolUse {
                        id: "call_signed".to_string(),
                        name: "web_search".to_string(),
                        input: serde_json::json!({ "q": "leviath" }),
                        thought_signature: Some("sig-abc".to_string()),
                    },
                    ContentBlock::ToolUse {
                        id: "call_unsigned".to_string(),
                        name: "read_file".to_string(),
                        input: serde_json::json!({ "path": "x" }),
                        thought_signature: None,
                    },
                    // Both answered: an unanswered call is dropped before the
                    // fold ever sees it, so it would prove nothing here.
                    ContentBlock::ToolResult {
                        tool_use_id: "call_signed".to_string(),
                        content: "results".to_string(),
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "call_unsigned".to_string(),
                        content: "file body".to_string(),
                        is_error: false,
                    },
                ]),
                cache_breakpoint: false,
            }],
            model: "gemini-3.1-pro-preview".to_string(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let msgs = openai_messages(&request);
        let wire = serde_json::to_string(&msgs).expect("serialises");
        assert!(
            wire.contains("sig-abc"),
            "the signed call keeps its signature"
        );
        assert!(wire.contains("web_search"), "and stays a call");
        assert!(
            wire.contains("[Earlier in this run I called read_file"),
            "while the unsigned one beside it is folded: {wire}"
        );
    }

    /// Arguments reach the fold as an object on the routes that send them that
    /// way, and as a JSON string on the rest. Both have to come back out as
    /// something the model can read.
    #[test]
    fn the_fold_quotes_object_arguments_too() {
        let request = InferenceRequest {
            system: vec![],
            messages: vec![Message {
                role: "assistant".to_string(),
                content: MessageContent::Blocks(vec![
                    ContentBlock::ToolUse {
                        id: "call_1".to_string(),
                        name: "read_file".to_string(),
                        input: serde_json::json!({ "path": "notes.md" }),
                        thought_signature: None,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "call_1".to_string(),
                        content: "the notes".to_string(),
                        is_error: false,
                    },
                ]),
                cache_breakpoint: false,
            }],
            model: "gemini-3.1-pro-preview".to_string(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let msgs = openai_messages_with(&request, ToolArgsFormat::Object);
        let wire = serde_json::to_string(&msgs).expect("serialises");
        assert!(!wire.contains("tool_calls"), "still folded: {wire}");
        assert!(
            wire.contains("notes.md"),
            "the arguments survive the fold as text: {wire}"
        );
        assert!(wire.contains("the notes"), "and so does the answer");
    }

    /// A model with no such rule is untouched, so this costs nothing anywhere
    /// else: the same unsigned call replays as a call.
    #[test]
    fn a_model_that_does_not_sign_calls_replays_them_unchanged() {
        let request = InferenceRequest {
            system: vec![],
            messages: vec![Message {
                role: "assistant".to_string(),
                content: MessageContent::Blocks(vec![
                    ContentBlock::ToolUse {
                        id: "call_1".to_string(),
                        name: "read_file".to_string(),
                        input: serde_json::json!({ "path": "x" }),
                        thought_signature: None,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "call_1".to_string(),
                        content: "file body".to_string(),
                        is_error: false,
                    },
                ]),
                cache_breakpoint: false,
            }],
            model: "gpt-5.5".to_string(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let wire = serde_json::to_string(&openai_messages(&request)).expect("serialises");
        assert!(
            wire.contains("tool_calls"),
            "untouched for a model with no rule"
        );
    }

    /// Gemini 3.x returns an opaque per-call `thought_signature` under
    /// `extra_content.google` and rejects a follow-up request that omits it.
    /// Capture on parse ...
    #[test]
    fn parse_captures_a_gemini_thought_signature() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": { "name": "list_dir", "arguments": "{}" },
                        "extra_content": { "google": { "thought_signature": "sig-bytes" } }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        });
        let resp = parse_openai_response(&body).unwrap();
        assert_eq!(
            resp.tool_calls[0].thought_signature.as_deref(),
            Some("sig-bytes")
        );

        // ... and absent stays absent (Anthropic/OpenAI shapes).
        let plain = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "c2",
                        "type": "function",
                        "function": { "name": "list_dir", "arguments": "{}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        });
        let resp = parse_openai_response(&plain).unwrap();
        assert_eq!(resp.tool_calls[0].thought_signature, None);
    }

    /// ... and replay on build: the signature goes back exactly where the
    /// provider put it, and a signature-less call gains no `extra_content`.
    #[test]
    fn request_replays_a_thought_signature_in_place() {
        let content = MessageContent::Blocks(vec![
            ContentBlock::ToolUse {
                id: "c1".into(),
                name: "list_dir".into(),
                input: serde_json::json!({}),
                thought_signature: Some("sig-bytes".into()),
            },
            ContentBlock::ToolUse {
                id: "c2".into(),
                name: "read_file".into(),
                input: serde_json::json!({}),
                thought_signature: None,
            },
        ]);
        let messages = message_to_openai("assistant", &content);
        let calls = messages[0]["tool_calls"].as_array().unwrap();
        assert_eq!(
            calls[0]["extra_content"]["google"]["thought_signature"],
            "sig-bytes"
        );
        assert!(
            calls[1].get("extra_content").is_none(),
            "no signature, no extra_content: {calls:?}"
        );
    }

    #[test]
    fn unpaired_tool_turns_are_dropped_from_the_request() {
        let call_only = InferenceRequest {
            system: vec![],
            messages: vec![
                Message {
                    role: "user".into(),
                    content: MessageContent::Text("do it".into()),
                    cache_breakpoint: false,
                },
                Message {
                    role: "assistant".into(),
                    content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                        id: "orphan".into(),
                        name: "search".into(),
                        input: serde_json::json!({}),
                        thought_signature: Some("sig".into()),
                    }]),
                    cache_breakpoint: false,
                },
            ],
            model: "gemini-3.5-flash".into(),
            max_tokens: 64,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::json!({}),
            request_timeout_secs: None,
        };
        let msgs = openai_messages(&call_only);
        assert_eq!(
            msgs.len(),
            1,
            "the unanswered call turn is dropped: {msgs:?}"
        );
        assert_eq!(msgs[0]["role"], "user");

        // A response with no call is equally invalid and equally dropped.
        let result_only = InferenceRequest {
            messages: vec![
                Message {
                    role: "user".into(),
                    content: MessageContent::Text("do it".into()),
                    cache_breakpoint: false,
                },
                Message {
                    role: "user".into(),
                    content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                        tool_use_id: "ghost".into(),
                        content: "stale".into(),
                        is_error: false,
                    }]),
                    cache_breakpoint: false,
                },
            ],
            ..call_only.clone()
        };
        let msgs = openai_messages(&result_only);
        assert_eq!(msgs.len(), 1, "the orphan response is dropped: {msgs:?}");

        // A properly paired exchange survives intact.
        let paired = InferenceRequest {
            messages: vec![
                Message {
                    role: "assistant".into(),
                    content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                        id: "c1".into(),
                        name: "search".into(),
                        input: serde_json::json!({}),
                        thought_signature: Some("sig".into()),
                    }]),
                    cache_breakpoint: false,
                },
                Message {
                    role: "user".into(),
                    content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                        tool_use_id: "c1".into(),
                        content: "found".into(),
                        is_error: false,
                    }]),
                    cache_breakpoint: false,
                },
            ],
            ..call_only.clone()
        };
        let msgs = openai_messages(&paired);
        // The pair survives; a leading user turn is prepended because the
        // conversation opens on an assistant turn (see `lead_with_a_user_turn`).
        assert_eq!(msgs.len(), 3, "a paired exchange survives: {msgs:?}");
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[2]["role"], "tool");
    }

    /// Gemini rejects a function-call turn that does not follow a user turn.
    /// Leviath's task lives in a pinned region (so it assembles into the
    /// system prompt), which left the assistant's opening tool call as the
    /// first message and killed every Gemini run on its second inference.
    #[test]
    fn a_conversation_opening_on_an_assistant_turn_gets_a_user_turn_first() {
        let req = InferenceRequest {
            system: vec![SystemBlock {
                text: "You are a coding agent. Task: add a doc comment.".into(),
                cache_hint: leviath_core::CacheHint::Never,
                volatility: leviath_core::Volatility::default(),
                region: String::new(),
            }],
            messages: vec![Message {
                role: "assistant".into(),
                content: MessageContent::Blocks(vec![
                    ContentBlock::ToolUse {
                        id: "c1".into(),
                        name: "list_dir".into(),
                        input: serde_json::json!({}),
                        thought_signature: Some("sig".into()),
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "c1".into(),
                        content: "main.rs".into(),
                        is_error: false,
                    },
                ]),
                cache_breakpoint: false,
            }],
            model: "gemini-3.5-flash".into(),
            max_tokens: 64,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::json!({}),
            request_timeout_secs: None,
        };
        let msgs = openai_messages(&req);
        let roles: Vec<&str> = msgs.iter().filter_map(|m| m["role"].as_str()).collect();
        assert_eq!(
            roles,
            vec!["system", "user", "assistant", "tool"],
            "a call turn must follow a user turn: {msgs:?}"
        );
    }

    /// An ordinary conversation already starting with a user turn is untouched.
    #[test]
    fn a_conversation_already_starting_with_a_user_turn_is_unchanged() {
        let req = InferenceRequest {
            system: vec![],
            messages: vec![Message {
                role: "user".into(),
                content: MessageContent::Text("hello".into()),
                cache_breakpoint: false,
            }],
            model: "gemini-3.5-flash".into(),
            max_tokens: 64,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::json!({}),
            request_timeout_secs: None,
        };
        assert_eq!(openai_messages(&req).len(), 1);
    }

    /// A call turn directly after a function response turn is legal - two
    /// back-to-back exchanges must not accrete filler user turns.
    #[test]
    fn a_call_turn_after_a_tool_turn_is_left_alone() {
        let exchange = |n: u32| Message {
            role: "assistant".into(),
            content: MessageContent::Blocks(vec![
                ContentBlock::ToolUse {
                    id: format!("c{n}"),
                    name: "list_dir".into(),
                    input: serde_json::json!({}),
                    thought_signature: Some("sig".into()),
                },
                ContentBlock::ToolResult {
                    tool_use_id: format!("c{n}"),
                    content: "ok".into(),
                    is_error: false,
                },
            ]),
            cache_breakpoint: false,
        };
        let req = InferenceRequest {
            system: vec![],
            messages: vec![
                Message {
                    role: "user".into(),
                    content: MessageContent::Text("go".into()),
                    cache_breakpoint: false,
                },
                exchange(1),
                exchange(2),
            ],
            model: "gemini-3.5-flash".into(),
            max_tokens: 64,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::json!({}),
            request_timeout_secs: None,
        };
        let roles: Vec<String> = openai_messages(&req)
            .iter()
            .filter_map(|m| m["role"].as_str().map(str::to_string))
            .collect();
        assert_eq!(
            roles,
            vec!["user", "assistant", "tool", "assistant", "tool"],
            "no filler between a tool turn and the next call"
        );
    }

    /// The mid-conversation variant of the turn-order rule: an assistant text
    /// turn directly before a call turn (a carried stage response) is just as
    /// illegal to Gemini as a leading one, and killed real runs at their first
    /// stage transition.
    #[test]
    fn a_call_turn_after_an_assistant_text_turn_gets_a_user_turn_between() {
        let req = InferenceRequest {
            system: vec![],
            messages: vec![
                Message {
                    role: "user".into(),
                    content: MessageContent::Text("start".into()),
                    cache_breakpoint: false,
                },
                Message {
                    role: "assistant".into(),
                    content: MessageContent::Text("analysis so far".into()),
                    cache_breakpoint: false,
                },
                Message {
                    role: "assistant".into(),
                    content: MessageContent::Blocks(vec![
                        ContentBlock::ToolUse {
                            id: "c1".into(),
                            name: "list_dir".into(),
                            input: serde_json::json!({}),
                            thought_signature: Some("sig".into()),
                        },
                        ContentBlock::ToolResult {
                            tool_use_id: "c1".into(),
                            content: "main.rs".into(),
                            is_error: false,
                        },
                    ]),
                    cache_breakpoint: false,
                },
            ],
            model: "gemini-3.5-flash".into(),
            max_tokens: 64,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::json!({}),
            request_timeout_secs: None,
        };
        let msgs = openai_messages(&req);
        let roles: Vec<&str> = msgs.iter().filter_map(|m| m["role"].as_str()).collect();
        assert_eq!(
            roles,
            vec!["user", "assistant", "user", "assistant", "tool"],
            "a user turn separates text from the call: {msgs:?}"
        );
    }

    /// One block list carrying both a call and its result must emit both; the
    /// results used to be dropped, producing the unanswered-call shape.
    #[test]
    fn a_block_with_both_a_call_and_its_result_emits_both() {
        let content = MessageContent::Blocks(vec![
            ContentBlock::ToolUse {
                id: "c9".into(),
                name: "search".into(),
                input: serde_json::json!({}),
                thought_signature: None,
            },
            ContentBlock::ToolResult {
                tool_use_id: "c9".into(),
                content: "answer".into(),
                is_error: false,
            },
        ]);
        let messages = message_to_openai("assistant", &content);
        assert_eq!(messages.len(), 2, "call and result: {messages:?}");
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "c9");
    }

    #[test]
    fn build_request_body_blocks_text_only() {
        let req = InferenceRequest {
            system: vec![],
            messages: vec![Message {
                role: "user".into(),
                content: MessageContent::Blocks(vec![
                    ContentBlock::Text {
                        text: "Hello ".into(),
                    },
                    ContentBlock::Text {
                        text: "world".into(),
                    },
                ]),
                cache_breakpoint: false,
            }],
            model: "gpt-4".into(),
            max_tokens: 256,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::json!({}),
            request_timeout_secs: None,
        };
        let body = build_openai_request_body(&req);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "Hello world");
    }

    #[test]
    fn build_request_body_system_blocks_prepended() {
        let req = InferenceRequest {
            system: vec![
                crate::provider::SystemBlock {
                    text: "You are helpful.".into(),
                    cache_hint: leviath_core::CacheHint::Always,
                    volatility: leviath_core::Volatility::default(),
                    region: String::new(),
                },
                crate::provider::SystemBlock {
                    text: "Be concise.".into(),
                    cache_hint: leviath_core::CacheHint::Always,
                    volatility: leviath_core::Volatility::default(),
                    region: String::new(),
                },
            ],
            messages: vec![Message {
                role: "user".into(),
                content: "Hi".into(),
                cache_breakpoint: false,
            }],
            model: "gpt-4".into(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::json!({}),
            request_timeout_secs: None,
        };
        let body = build_openai_request_body(&req);
        let messages = body["messages"].as_array().unwrap();
        // Both blocks, joined into the one system message the chat shape has.
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You are helpful.\n\nBe concise.");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "Hi");
    }

    /// However many blocks the context assembles into, exactly one system
    /// message goes out.
    ///
    /// Some Ollama chat templates reject the second one - qwen3.8 answers
    /// `HTTP 500 {"error":"system message must be at the beginning"}`, which is
    /// a misleading way to say "at most one". An agent with several pinned
    /// regions (deep-researcher has eight) could not take a single turn against
    /// it, and every stage failed on its first call.
    #[test]
    fn many_system_blocks_become_exactly_one_system_message() {
        let req = InferenceRequest {
            system: (0..8)
                .map(|i| SystemBlock {
                    text: format!("block {i}"),
                    cache_hint: leviath_core::CacheHint::Always,
                    volatility: leviath_core::Volatility::default(),
                    region: String::new(),
                })
                .collect(),
            messages: vec![Message {
                role: "user".into(),
                content: "Hi".into(),
                cache_breakpoint: false,
            }],
            model: "qwen3.8".into(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::json!({}),
            request_timeout_secs: None,
        };
        let messages = openai_messages_with(&req, ToolArgsFormat::Object);
        let systems = messages.iter().filter(|m| m["role"] == "system").count();
        assert_eq!(systems, 1, "{messages:#?}");
        let joined = (0..8)
            .map(|i| format!("block {i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        assert_eq!(
            messages[0]["content"], joined,
            "every block survives the join"
        );
    }

    /// A pinned region that happens to be empty this turn contributes nothing,
    /// rather than a blank paragraph in the middle of the prompt.
    #[test]
    fn empty_system_blocks_are_dropped_rather_than_joined() {
        let req = InferenceRequest {
            system: vec![
                SystemBlock {
                    text: "real".into(),
                    cache_hint: leviath_core::CacheHint::Always,
                    volatility: leviath_core::Volatility::default(),
                    region: String::new(),
                },
                SystemBlock {
                    text: "   ".into(),
                    cache_hint: leviath_core::CacheHint::Always,
                    volatility: leviath_core::Volatility::default(),
                    region: String::new(),
                },
                SystemBlock {
                    text: "also real".into(),
                    cache_hint: leviath_core::CacheHint::Always,
                    volatility: leviath_core::Volatility::default(),
                    region: String::new(),
                },
            ],
            messages: vec![Message {
                role: "user".into(),
                content: "Hi".into(),
                cache_breakpoint: false,
            }],
            model: "m".into(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::json!({}),
            request_timeout_secs: None,
        };
        let messages = openai_messages_with(&req, ToolArgsFormat::Object);
        assert_eq!(messages[0]["content"], "real\n\nalso real");
    }

    /// A request with no system content at all sends no system message, rather
    /// than an empty one.
    #[test]
    fn no_system_blocks_means_no_system_message() {
        let req = InferenceRequest {
            system: vec![],
            messages: vec![Message {
                role: "user".into(),
                content: "Hi".into(),
                cache_breakpoint: false,
            }],
            model: "m".into(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::json!({}),
            request_timeout_secs: None,
        };
        let messages = openai_messages_with(&req, ToolArgsFormat::Object);
        assert!(
            messages.iter().all(|m| m["role"] != "system"),
            "{messages:#?}"
        );
    }

    #[test]
    fn parse_response_tool_call_no_function_key() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_x"
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });
        let resp = parse_openai_response(&body).unwrap();
        assert_eq!(resp.tool_calls.len(), 1);
        // function defaults to null, so name and arguments use defaults
        assert_eq!(resp.tool_calls[0].id, "call_x");
        assert_eq!(resp.tool_calls[0].name, "");
        assert!(resp.tool_calls[0].arguments.is_object());
    }

    #[test]
    fn parse_response_tool_call_no_id_field() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "function": {
                            "name": "do_thing",
                            "arguments": "{\"a\":1}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });
        let resp = parse_openai_response(&body).unwrap();
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "");
        assert_eq!(resp.tool_calls[0].name, "do_thing");
        assert_eq!(resp.tool_calls[0].arguments["a"], 1);
    }

    #[tokio::test]
    async fn openai_sse_stream_propagates_real_reqwest_error() {
        use tokio_stream::StreamExt;

        let url = leviath_testkit::spawn_mock_server_truncated_body(200, "OK").await;
        let client = reqwest::Client::new();
        let resp = client.get(&url).send().await.unwrap();
        let byte_stream = resp.bytes_stream();
        let mut sse = OpenAiSseStream::new(byte_stream);

        let item = sse.next().await.expect("stream should yield an item");
        let err = item.unwrap_err();
        assert!(err.to_string().contains("Request failed:"));
    }

    async fn spawn_ok_server() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let resp =
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
            let _ = socket.write_all(resp).await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        });
        format!("http://{}", addr)
    }

    #[tokio::test]
    async fn send_chat_request_success_with_limiter_resets_backoff() {
        // A 200 response on the limiter-carrying path drives the success
        // branch, including the `if let Some(limiter)` reset_backoff call.
        let url = spawn_ok_server().await;
        let client = reqwest::Client::new();
        let limiter = crate::rate_limit::RateLimiter::with_defaults();
        let body = serde_json::json!({ "model": "x" });
        let resp = send_chat_request(
            &client,
            "test",
            &url,
            &[("content-type", "application/json".to_string())],
            &body,
            Some(&limiter),
            None,
        )
        .await;
        assert!(resp.is_ok());
    }

    #[tokio::test]
    async fn send_chat_request_tolerates_unparseable_debug_header() {
        // Under the `debug-http` feature (which the coverage build enables),
        // building the debug header map skips any header whose name/value
        // won't parse (the `if let (Ok, Ok)` false arm) rather than panicking.
        // The request send itself fails (unroutable address, and reqwest also
        // rejects the bad header), which is fine - the debug-http block runs
        // first, before the send.
        let client = reqwest::Client::new();
        let body = serde_json::json!({ "model": "x" });
        let _ = send_chat_request(
            &client,
            "test",
            "http://127.0.0.1:1/v1/chat/completions",
            &[
                ("bad header name", "v".to_string()),
                ("content-type", "application/json".to_string()),
            ],
            &body,
            None,
            None,
        )
        .await;
    }

    // ─── The output-cap key is per provider ─────────────────────────────────

    #[test]
    fn a_compatibility_server_still_gets_max_tokens() {
        let req = sample_request();
        let body = build_openai_request_body(&req);
        assert_eq!(body["max_tokens"], 1024);
        assert!(
            body.get("max_completion_tokens").is_none(),
            "a compatibility server must not be sent the OpenAI-only key"
        );
    }

    #[test]
    fn openai_gets_max_completion_tokens_instead() {
        // OpenAI rejects `max_tokens` outright on every current model:
        // HTTP 400 unsupported_parameter. Sending both would be rejected too.
        let req = sample_request();
        let body = build_openai_request_body_with(&req, TokenLimitField::MaxCompletionTokens);
        assert_eq!(body["max_completion_tokens"], 1024);
        assert!(
            body.get("max_tokens").is_none(),
            "OpenAI must not be sent the key it rejects"
        );
    }

    #[test]
    fn each_variant_names_its_own_key() {
        assert_eq!(TokenLimitField::MaxTokens.key(), "max_tokens");
        assert_eq!(
            TokenLimitField::MaxCompletionTokens.key(),
            "max_completion_tokens"
        );
    }

    // ─── Tool-call arguments are spelled per dialect ────────────────────────

    fn one_tool_use() -> MessageContent {
        MessageContent::Blocks(vec![ContentBlock::ToolUse {
            id: "call_1".to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({ "path": "notes.txt" }),
            thought_signature: None,
        }])
    }

    #[test]
    fn openai_gets_tool_arguments_as_a_json_string() {
        let msgs = message_to_openai("assistant", &one_tool_use());
        let args = &msgs[0]["tool_calls"][0]["function"]["arguments"];
        assert!(args.is_string());
        assert_eq!(args.as_str().unwrap_or_default(), r#"{"path":"notes.txt"}"#);
    }

    #[test]
    fn ollama_gets_tool_arguments_as_an_object() {
        // Ollama's Go server types this field as ToolCallFunctionArguments and
        // rejects the string spelling, so a second turn - the first one with
        // history to replay - failed with HTTP 400.
        let msgs = message_to_openai_with("assistant", &one_tool_use(), ToolArgsFormat::Object);
        let args = &msgs[0]["tool_calls"][0]["function"]["arguments"];
        assert!(args.is_object());
        assert_eq!(args["path"], "notes.txt");
    }

    #[test]
    fn each_arg_format_renders_its_own_shape() {
        let input = serde_json::json!({ "a": 1 });
        assert!(ToolArgsFormat::JsonString.render(&input).is_string());
        assert!(ToolArgsFormat::Object.render(&input).is_object());
    }

    #[test]
    fn the_arg_format_reaches_the_messages_a_provider_actually_sends() {
        // Regression: `openai_messages_with` took the format and then called the
        // unparameterised `message_to_openai`, so the argument was dead and
        // Ollama kept getting the string it rejects. A test on the leaf function
        // passed the whole time - this one asserts the path.
        let req = InferenceRequest {
            // The result has to be here too: `drop_unpaired_tool_turns`
            // discards a call with no answer, which would empty the array and
            // make this assert on nothing.
            messages: vec![
                Message {
                    role: "assistant".to_string(),
                    content: one_tool_use(),
                    cache_breakpoint: false,
                },
                Message {
                    role: "user".to_string(),
                    content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                        tool_use_id: "call_1".to_string(),
                        content: "the access code is 7731".to_string(),
                        is_error: false,
                    }]),
                    cache_breakpoint: false,
                },
            ],
            ..sample_request()
        };
        let msgs = openai_messages_with(&req, ToolArgsFormat::Object);
        let call = msgs
            .iter()
            .find_map(|m| m.get("tool_calls").and_then(|t| t.get(0)))
            .expect("the assistant turn carries a tool call");
        assert!(
            call["function"]["arguments"].is_object(),
            "the format did not reach the emitted message: {call}"
        );

        let msgs = openai_messages_with(&req, ToolArgsFormat::JsonString);
        let call = msgs
            .iter()
            .find_map(|m| m.get("tool_calls").and_then(|t| t.get(0)))
            .expect("the assistant turn carries a tool call");
        assert!(call["function"]["arguments"].is_string());
    }
}
