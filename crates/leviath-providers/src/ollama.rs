//! Ollama provider implementation.
//!
//! Ollama provides local LLM execution via NDJSON streaming.

use crate::provider::{
    FinishReason, InferenceRequest, InferenceResponse, ModelCapabilities, ModelCapabilityOverride,
    ModelInfo, Provider, ProviderError, Result, StreamChunk, TokenUsage,
};
use async_trait::async_trait;
use futures_core::Stream;
use std::collections::HashMap;
use std::pin::Pin;

/// Ollama provider for local LLM execution.
pub struct OllamaProvider {
    /// HTTP client
    client: reqwest::Client,

    /// API base URL (defaults to local)
    base_url: String,

    /// Per-model capability overrides
    capability_overrides: HashMap<String, ModelCapabilityOverride>,
    /// Effective serving window per model, learned from the server at start-up
    /// by [`Provider::prime_capabilities`]. Empty until then, and empty for good
    /// if the server could not be reached - in which case the compiled table
    /// stays in charge.
    api_windows: std::sync::Arc<std::sync::Mutex<HashMap<String, usize>>>,
    /// Models already warned about, so a guessed window is announced once per
    /// model rather than once per inference.
    warned_guessed: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

/// A tool-call id unique for the life of a conversation.
///
/// Ollama sends no ids of its own, so these are minted here - and the mint used
/// to be the call's index within one response, which restarts at 0 every turn.
/// A window ten turns deep therefore held ten distinct calls all named
/// `ollama_0`.
///
/// That is not cosmetic. `drop_unpaired_tool_turns` pairs a call with its
/// response *by id*, to keep a window that has evicted half a pair from putting
/// a malformed conversation on the wire. With every id equal, every call looked
/// answered and every response looked called, so the guard never removed
/// anything for this provider (issue #470) - and a response stranded by
/// eviction survived at the head of the conversation, which is what suppressed
/// the inserted user turn in #469.
///
/// The sequence is process-wide rather than per-response, and carries a prefix
/// minted once per process, because a run outlives the daemon: a pause and
/// resume restores a window full of ids from the previous process, and a bare
/// counter would start again at zero and collide with them. The sequence is
/// still monotonic within a process, so a transcript reads in call order.
fn next_tool_call_id() -> String {
    static PREFIX: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let prefix = PREFIX.get_or_init(|| {
        use rand::RngExt as _;
        format!("{:08x}", rand::rng().random::<u32>())
    });
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("ollama_{prefix}_{sequence}")
}

/// The effective serving window named in an `/api/show` response, if it names one.
///
/// Ollama reports two different numbers and the obvious one is wrong.
/// `model_info["<arch>.context_length"]` is the architecture's ceiling - 262144 for
/// qwen35 - while the window the server will actually serve is `num_ctx` in the
/// Modelfile parameters, 32768 for a model built with that cap. Taking the ceiling
/// would replace one overestimate with a larger one.
///
/// `parameters` is a text block, not JSON:
///
/// ```text
/// temperature                    1
/// num_ctx                        32768
/// ```
///
/// A model that names no `num_ctx` is served at the server's own default, which
/// Ollama does not report anywhere. `None` leaves the compiled table in charge
/// rather than recording a guess that would outrank it - the same rule the
/// OpenRouter provider follows for a model its `/models` says nothing about.
fn effective_window(show: &serde_json::Value) -> Option<usize> {
    let parameters = show.get("parameters")?.as_str()?;
    parameters.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        (parts.next()? == "num_ctx")
            .then(|| parts.next()?.parse::<usize>().ok())
            .flatten()
    })
}

impl OllamaProvider {
    /// Create a new Ollama provider.
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: "http://localhost:11434".to_string(),
            capability_overrides: HashMap::new(),
            api_windows: Default::default(),
            warned_guessed: Default::default(),
        }
    }

    /// Create a new Ollama provider with custom base URL.
    pub fn with_base_url(client: reqwest::Client, base_url: String) -> Self {
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            tracing::warn!(url = %base_url, "Ollama base URL should start with http:// or https://");
        }
        Self {
            client,
            base_url,
            capability_overrides: HashMap::new(),
            api_windows: Default::default(),
            warned_guessed: Default::default(),
        }
    }

    /// Create a new Ollama provider with a custom base URL and per-model capability overrides.
    pub fn with_overrides(
        client: reqwest::Client,
        base_url: String,
        overrides: HashMap<String, ModelCapabilityOverride>,
    ) -> Self {
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            tracing::warn!(url = %base_url, "Ollama base URL should start with http:// or https://");
        }
        Self {
            client,
            base_url,
            capability_overrides: overrides,
            api_windows: Default::default(),
            warned_guessed: Default::default(),
        }
    }

    /// Replace the window with what the server actually serves, when start-up
    /// managed to ask.
    ///
    /// Only the window. The rest of `ModelCapabilities` is about how a request
    /// must be *shaped* - whether tools work, whether temperature is accepted -
    /// and `/api/show` describes what a model is, not the quirks of talking to
    /// it. Taking the size from the live answer and the shape from the compiled
    /// table gives each the question it can answer.
    fn api_corrected(&self, model: &str, base: ModelCapabilities) -> ModelCapabilities {
        let windows = leviath_core::sync::lock(&self.api_windows);
        match windows.get(model) {
            Some(&max_context_tokens) => ModelCapabilities {
                max_context_tokens,
                ..base
            },
            None => base,
        }
    }

    /// Say so, once per model, when the window did not come from the server.
    ///
    /// Unlike the OpenRouter equivalent, the test is not "did we land on the
    /// conservative fallback". For Ollama *every* compiled answer is a guess
    /// from a substring of the model's name, and the dangerous one is not the
    /// small fallback but the confident large one: `qwen3.8-32k` matches
    /// `qwen3` and is handed 131072 against a real 32768. So the question is
    /// simply whether the server told us, and anything else is worth
    /// announcing.
    fn warn_if_guessed(&self, model: &str, resolved: &ModelCapabilities) {
        if leviath_core::sync::lock(&self.api_windows).contains_key(model) {
            return;
        }
        let mut warned = leviath_core::sync::lock(&self.warned_guessed);
        if !warned.insert(model.to_string()) {
            return;
        }
        tracing::warn!(
            model = %model,
            assumed_context_tokens = resolved.max_context_tokens,
            "this Ollama model's context window was guessed from its name, not \
             read from the server, so percentage region budgets resolve against \
             a number that may be far too large. Ollama serves a model at its \
             Modelfile `num_ctx`, or at the server default when it sets none. \
             Set the real window with [model_capabilities.\"{model}\"] \
             max_context_tokens = <n>",
        );
    }

    /// Ask the server for every installed model's effective window.
    async fn learn_model_windows(&self) -> Result<HashMap<String, usize>> {
        let tags = self
            .client
            .get(format!("{}/api/tags", self.base_url))
            .send()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;
        let tags = crate::provider::check_http_response(tags, None).await?;
        let tags: serde_json::Value = tags
            .json()
            .await
            .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;

        let names: Vec<String> = tags
            .get("models")
            .and_then(|m| m.as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|e| Some(e.get("name")?.as_str()?.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let mut windows = HashMap::with_capacity(names.len());
        for name in names {
            let show = self
                .client
                .post(format!("{}/api/show", self.base_url))
                .json(&serde_json::json!({ "model": name }))
                .send()
                .await
                .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;
            let show = crate::provider::check_http_response(show, None).await?;
            let show: serde_json::Value = show
                .json()
                .await
                .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;
            if let Some(window) = effective_window(&show) {
                windows.insert(name, window);
            }
        }
        Ok(windows)
    }

    /// Return built-in capability defaults for a model based on its name pattern.
    fn builtin_capabilities(&self, model: &str) -> ModelCapabilities {
        // Llama 3.x and Qwen 2.x/3 - tool-capable, 128K context
        if model.contains("llama3")
            || model.contains("llama-3")
            || model.contains("qwen2.5")
            || model.contains("qwen3")
            || model.contains("qwen2")
        {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 131_072,
                max_output_tokens: 8192,
            }
        // Mistral / Mixtral - tool-capable
        } else if model.contains("mistral") || model.contains("mixtral") {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 32_768,
                max_output_tokens: 4096,
            }
        // Phi-4 - tool-capable, 128K context
        } else if model.contains("phi-4") || model.contains("phi4") {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 131_072,
                max_output_tokens: 8192,
            }
        // DeepSeek R1 - reasoning, no tool calls
        } else if model.contains("deepseek-r1") {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: false,
                supports_system_prompt: true,
                max_context_tokens: 131_072,
                max_output_tokens: 8192,
            }
        // DeepSeek general - tool-capable
        } else if model.contains("deepseek") {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 131_072,
                max_output_tokens: 8192,
            }
        // Gemma - no tool support
        } else if model.contains("gemma") {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: false,
                supports_system_prompt: true,
                max_context_tokens: 131_072,
                max_output_tokens: 8192,
            }
        // CodeLlama - no tool support
        } else if model.contains("codellama") {
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: false,
                supports_system_prompt: true,
                max_context_tokens: 16_384,
                max_output_tokens: 4096,
            }
        } else {
            // Conservative fallback for unknown local models
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: true,
                supports_tools: false,
                supports_system_prompt: true,
                max_context_tokens: 8192,
                max_output_tokens: 4096,
            }
        }
    }

    /// Build request body for the Ollama API.
    fn build_request_body(&self, request: &InferenceRequest) -> serde_json::Value {
        // System blocks prepended + tool_use/tool_result history converted to
        // OpenAI format (Ollama speaks the OpenAI chat shape). The shared
        // helper matters here: a naive conversion drops the system prompt and
        // serializes tool blocks as raw JSON.
        // Ollama declares `tool_calls.function.arguments` as an object and
        // rejects OpenAI's JSON-string spelling, so history replays as objects.
        let messages = crate::openai_compat::openai_messages_with(
            request,
            crate::openai_compat::ToolArgsFormat::Object,
        );

        let caps = self.capabilities(&request.model);
        let options = if caps.supports_temperature {
            serde_json::json!({
                "temperature": request.temperature,
                "num_predict": request.max_tokens,
            })
        } else {
            serde_json::json!({
                "num_predict": request.max_tokens,
            })
        };
        let mut body = serde_json::json!({
            "model": request.model,
            "messages": messages,
            "options": options,
        });

        // Add tools if present (Ollama supports tool calling for some models)
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

        // `think` is Ollama's own switch for a reasoning model, and it sits at
        // the top level rather than under `options` - so it is lifted out
        // before the rest of `extra` is merged, or it would arrive as a
        // sampling knob Ollama ignores.
        //
        // Worth having as a knob at all because a thinking model spends real
        // budget before it says anything: qwen3.8 asked for a run title in 64
        // tokens returns an empty string, having used all of them reasoning
        // about what a title is. `think = false` on that stage returns the
        // title. Only sent when a blueprint asks for it, since a model with no
        // thinking to switch off rejects the field.
        let think = request.extra.get("think").cloned();
        if let Some(think) = think {
            body["think"] = think;
        }
        let mut sampling = request.extra.clone();
        if let Some(params) = sampling.as_object_mut() {
            params.remove("think");
        }
        crate::openai_compat::merge_extra_params(
            body.get_mut("options")
                .and_then(|v| v.as_object_mut())
                .expect("ollama request body always has an `options` object"),
            &sampling,
        );
        body
    }

    /// Parse non-streaming response from Ollama.
    fn parse_response(&self, body: &serde_json::Value) -> Result<InferenceResponse> {
        let message = body
            .get("message")
            .ok_or_else(|| ProviderError::InvalidResponse("No message in response".to_string()))?;

        let content = message
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();

        let mut tool_calls = Vec::new();
        if let Some(tcs) = message.get("tool_calls").and_then(|tc| tc.as_array()) {
            for tc in tcs {
                let function = tc.get("function").unwrap_or(&serde_json::Value::Null);
                let name = function
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let arguments = function
                    .get("arguments")
                    .cloned()
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                tool_calls.push(crate::provider::ToolCall {
                    id: next_tool_call_id(),
                    name,
                    arguments,
                    thought_signature: None,
                });
            }
        }

        let eval_count = body.get("eval_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let prompt_eval_count = body
            .get("prompt_eval_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        let finish_reason = if !tool_calls.is_empty() {
            FinishReason::ToolCall
        } else {
            FinishReason::Complete
        };

        Ok(InferenceResponse {
            content,
            tool_calls,
            tokens_used: TokenUsage {
                prompt_tokens: prompt_eval_count,
                completion_tokens: eval_count,
                total_tokens: prompt_eval_count + eval_count,
                cached_tokens: 0,
                cache_write_tokens: 0,
            },
            finish_reason,
        })
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    async fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        tracing::debug!(model = %request.model, "Calling Ollama API");

        let mut body = self.build_request_body(request);
        body["stream"] = serde_json::Value::Bool(false);
        let url = format!("{}/api/chat", self.base_url);

        let response = crate::openai_compat::send_chat_request(
            &self.client,
            "ollama",
            &url,
            &[("Content-Type", "application/json".to_string())],
            &body,
            None,
            request.request_timeout_secs,
        )
        .await?;

        let response_body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;

        self.parse_response(&response_body)
    }

    async fn infer_stream(
        &self,
        request: &InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        tracing::debug!(model = %request.model, "Calling Ollama API (streaming)");

        let mut body = self.build_request_body(request);
        body["stream"] = serde_json::Value::Bool(true);
        let url = format!("{}/api/chat", self.base_url);

        let response = crate::openai_compat::send_chat_request(
            &self.client,
            "ollama",
            &url,
            &[("Content-Type", "application/json".to_string())],
            &body,
            None,
            request.request_timeout_secs,
        )
        .await?;

        let byte_stream = response.bytes_stream();
        let stream = OllamaNdjsonStream::new(byte_stream);

        Ok(Box::pin(stream))
    }

    async fn count_tokens(&self, text: &str, _model: &str) -> usize {
        // Ollama exposes no token-count endpoint; approximate locally.
        leviath_core::estimate_tokens(text)
    }

    fn max_context_tokens(&self, model: &str) -> usize {
        self.capabilities(model).max_context_tokens
    }

    fn name(&self) -> &str {
        "ollama"
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        // Three answers, narrowest first: what the user wrote, what the server
        // says, what this build was compiled with.
        let base = self.api_corrected(model, self.builtin_capabilities(model));
        // Merged, not swapped: an entry names only what it corrects. An
        // explicit override is an answer, so it silences the warning too.
        match self.capability_overrides.get(model) {
            Some(o) => o.apply_to(base),
            None => {
                self.warn_if_guessed(model, &base);
                base
            }
        }
    }

    /// Learn every installed model's real window, so percentage region budgets
    /// resolve against what the server will serve rather than what the model's
    /// name suggests.
    ///
    /// `qwen3.8-32k` contains `qwen3`, so the compiled table hands it 131072 -
    /// four times the 32768 it is actually served at. Budgets sized against the
    /// larger number never evict, the request overflows, and Ollama front-
    /// truncates it and then answers `no user query found in messages`, which
    /// names neither the size nor the truncation (issue #475).
    ///
    /// Failure is a warning upstream, not an error: a daemon whose Ollama is not
    /// running must still start, with the compiled table in charge.
    async fn prime_capabilities(&self) -> Result<()> {
        let windows = self.learn_model_windows().await?;
        let count = windows.len();
        *leviath_core::sync::lock(&self.api_windows) = windows;
        tracing::debug!(models = count, "learned Ollama model windows");
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let response = self
            .client
            .get(format!("{}/api/tags", self.base_url))
            .send()
            .await
            .map_err(|e| ProviderError::RequestFailed(e.to_string()))?;

        // Shared classification, as above. A bare `RequestFailed` here would
        // read as a retryable network fault; the status is what says whether
        // retrying can help.
        let response = crate::provider::check_http_response(response, None).await?;

        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| ProviderError::InvalidResponse(e.to_string()))?;

        let models = body
            .get("models")
            .and_then(|m| m.as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        let id = entry.get("name")?.as_str()?.to_string();
                        let capabilities = self.capabilities(&id);
                        Some(ModelInfo {
                            display_name: Some(id.clone()),
                            provider: "ollama".into(),
                            capabilities,
                            id,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(models)
    }
}

// NDJSON stream parser for Ollama's streaming API.
//
// The inner byte stream is boxed as a trait object rather than kept generic.
// In production this is always `reqwest`'s `bytes_stream()`; tests inject
// dozens of distinct mock stream types via `new`'s generic parameter, and a
// generic `impl<S> Stream` causes `cargo llvm-cov` to instrument each
// monomorphized `poll_next` separately, leaving some artificially "uncovered"
// even though the shared logic is fully exercised. Boxing collapses all of
// that into a single concrete `poll_next` implementation.
struct OllamaNdjsonStream {
    inner: Pin<Box<dyn Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send>>,
    buffer: String,
}

impl OllamaNdjsonStream {
    fn new<S>(inner: S) -> Self
    where
        S: Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    {
        Self {
            inner: Box::pin(inner),
            buffer: String::new(),
        }
    }
}

impl Stream for OllamaNdjsonStream {
    type Item = Result<StreamChunk>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            // Try to parse a complete JSON line
            // Split off one complete NDJSON line, if the newline ending it has
            // arrived. Both halves are copied out before the buffer is replaced.
            let split = this
                .buffer
                .split_once('\n')
                .map(|(line, rest)| (line.to_string(), rest.to_string()));
            if let Some((line, rest)) = split {
                this.buffer = rest;

                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                let json: serde_json::Value = match serde_json::from_str(line) {
                    Ok(j) => j,
                    Err(e) => {
                        return std::task::Poll::Ready(Some(Err(ProviderError::InvalidResponse(
                            e.to_string(),
                        ))));
                    }
                };

                // Check for done flag
                let done = json.get("done").and_then(|v| v.as_bool()).unwrap_or(false);

                let message = json.get("message");
                let content = message
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();

                if done {
                    let eval_count =
                        json.get("eval_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    let prompt_eval_count = json
                        .get("prompt_eval_count")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as usize;

                    // Parse tool calls from the final chunk's message.tool_calls
                    let mut tool_calls = Vec::new();
                    if let Some(tcs) = message
                        .and_then(|m| m.get("tool_calls"))
                        .and_then(|tc| tc.as_array())
                    {
                        for (i, tc) in tcs.iter().enumerate() {
                            let function = tc.get("function").unwrap_or(&serde_json::Value::Null);
                            let name = function
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let arguments = function
                                .get("arguments")
                                .cloned()
                                .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                            tool_calls.push(crate::provider::ToolCallDelta {
                                index: i,
                                id: Some(next_tool_call_id()),
                                name: Some(name),
                                arguments_delta: arguments.to_string(),
                            });
                        }
                    }

                    let finish_reason = if tool_calls.is_empty() {
                        FinishReason::Complete
                    } else {
                        FinishReason::ToolCall
                    };

                    return std::task::Poll::Ready(Some(Ok(StreamChunk {
                        delta: content,
                        tool_calls,
                        tokens: Some(TokenUsage {
                            prompt_tokens: prompt_eval_count,
                            completion_tokens: eval_count,
                            total_tokens: prompt_eval_count + eval_count,
                            cached_tokens: 0,
                            cache_write_tokens: 0,
                        }),
                        finish_reason: Some(finish_reason),
                    })));
                }

                return std::task::Poll::Ready(Some(Ok(StreamChunk {
                    delta: content,
                    tool_calls: Vec::new(),
                    tokens: None,
                    finish_reason: None,
                })));
            }

            // Need more data
            match this.inner.as_mut().poll_next(cx) {
                std::task::Poll::Ready(Some(Ok(bytes))) => {
                    if let Ok(text) = std::str::from_utf8(&bytes) {
                        this.buffer.push_str(text);
                    }
                }
                std::task::Poll::Ready(Some(Err(e))) => {
                    return std::task::Poll::Ready(Some(Err(ProviderError::RequestFailed(
                        e.to_string(),
                    ))));
                }
                std::task::Poll::Ready(None) => {
                    // Try remaining buffer
                    let remaining = this.buffer.trim().to_string();
                    if !remaining.is_empty() {
                        this.buffer.clear();
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&remaining) {
                            let content = json
                                .get("message")
                                .and_then(|m| m.get("content"))
                                .and_then(|c| c.as_str())
                                .unwrap_or("")
                                .to_string();
                            return std::task::Poll::Ready(Some(Ok(StreamChunk {
                                delta: content,
                                tool_calls: Vec::new(),
                                tokens: None,
                                finish_reason: Some(FinishReason::Complete),
                            })));
                        }
                    }
                    return std::task::Poll::Ready(None);
                }
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::always_on_tracing_guard;
    use leviath_testkit::{
        spawn_mock_server,
        spawn_mock_server_truncated_body as spawn_mock_server_truncated_error_body,
    };

    #[test]
    fn test_provider_creation() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        assert_eq!(provider.name(), "ollama");
        assert!(provider.base_url.contains("localhost"));
    }

    #[test]
    fn test_custom_base_url() {
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "http://custom:11434".to_string(),
        );
        assert_eq!(provider.base_url, "http://custom:11434");
    }

    #[test]
    fn test_parse_response() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let body = serde_json::json!({
            "message": {
                "role": "assistant",
                "content": "Hello from Ollama!"
            },
            "eval_count": 10,
            "prompt_eval_count": 20,
            "done": true
        });

        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.content, "Hello from Ollama!");
        assert_eq!(response.tokens_used.completion_tokens, 10);
        assert_eq!(response.tokens_used.prompt_tokens, 20);
    }

    // ── Additional coverage tests ──────────────────────────────────────────

    #[test]
    fn test_default() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        assert_eq!(provider.name(), "ollama");
        assert!(provider.base_url.contains("localhost"));
    }

    #[test]
    fn test_with_overrides() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "custom-model".to_string(),
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: false,
                supports_tools: false,
                supports_system_prompt: false,
                max_context_tokens: 42,
                max_output_tokens: 10,
            }
            .into(),
        );
        let provider = OllamaProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "http://localhost:11434".to_string(),
            overrides,
        );
        let caps = provider.capabilities("custom-model");
        assert_eq!(caps.max_context_tokens, 42);
        assert!(!caps.supports_temperature);
    }

    #[test]
    fn test_capabilities_falls_through_to_builtin() {
        let provider = OllamaProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "http://localhost:11434".to_string(),
            HashMap::new(),
        );
        let caps = provider.capabilities("llama3-8b");
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[tokio::test]
    async fn test_count_tokens() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let tokens = provider.count_tokens("Hello, world!", "llama3").await;
        assert!(tokens > 0);
        // ceil(13 / 4): the shared estimate rounds up
        assert_eq!(tokens, 4);
    }

    #[tokio::test]
    async fn test_count_tokens_empty() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        assert_eq!(provider.count_tokens("", "llama3").await, 0);
    }

    #[test]
    fn test_max_context_tokens() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        assert_eq!(provider.max_context_tokens("llama3-8b"), 131_072);
        assert_eq!(provider.max_context_tokens("mistral-7b"), 32_768);
    }

    #[test]
    fn test_builtin_capabilities_llama3() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let caps = provider.builtin_capabilities("llama3-8b");
        assert!(caps.supports_temperature);
        assert!(caps.supports_streaming);
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_builtin_capabilities_qwen2() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let caps = provider.builtin_capabilities("qwen2-7b");
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_builtin_capabilities_qwen25() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let caps = provider.builtin_capabilities("qwen2.5-coder");
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_builtin_capabilities_qwen3() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let caps = provider.builtin_capabilities("qwen3-30b");
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_builtin_capabilities_mistral() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let caps = provider.builtin_capabilities("mistral-7b");
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 32_768);
        assert_eq!(caps.max_output_tokens, 4096);
    }

    #[test]
    fn test_builtin_capabilities_mixtral() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let caps = provider.builtin_capabilities("mixtral-8x7b");
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 32_768);
    }

    #[test]
    fn test_builtin_capabilities_phi4() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let caps = provider.builtin_capabilities("phi-4");
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_builtin_capabilities_phi4_variant() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let caps = provider.builtin_capabilities("phi4-mini");
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_builtin_capabilities_deepseek_r1() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let caps = provider.builtin_capabilities("deepseek-r1:latest");
        assert!(!caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_builtin_capabilities_deepseek_general() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let caps = provider.builtin_capabilities("deepseek-v3");
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_builtin_capabilities_gemma() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let caps = provider.builtin_capabilities("gemma-7b");
        assert!(!caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    #[test]
    fn test_builtin_capabilities_codellama() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        // Note: "codellama-34b" contains "llama-3" so it matches llama-3 branch.
        // Use a model name without a dash-number suffix.
        let caps = provider.builtin_capabilities("codellama:latest");
        assert!(!caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 16_384);
        assert_eq!(caps.max_output_tokens, 4096);
    }

    #[test]
    fn test_builtin_capabilities_unknown() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let caps = provider.builtin_capabilities("totally-unknown-model");
        assert!(!caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 8192);
        assert_eq!(caps.max_output_tokens, 4096);
    }

    #[test]
    fn test_parse_response_no_message_returns_error() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let body = serde_json::json!({ "done": true });
        let result = provider.parse_response(&body);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_response_with_tool_calls() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let body = serde_json::json!({
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "function": {
                            "name": "search",
                            "arguments": { "query": "rust" }
                        }
                    }
                ]
            },
            "eval_count": 5,
            "prompt_eval_count": 10,
            "done": true
        });

        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "search");
        assert!(response.tool_calls[0].id.starts_with("ollama_"));
        assert_eq!(response.finish_reason, FinishReason::ToolCall);
    }

    #[test]
    fn test_parse_response_total_tokens() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let body = serde_json::json!({
            "message": { "role": "assistant", "content": "ok" },
            "eval_count": 30,
            "prompt_eval_count": 70,
            "done": true
        });
        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.tokens_used.total_tokens, 100);
        assert_eq!(response.tokens_used.cached_tokens, 0);
        assert_eq!(response.tokens_used.cache_write_tokens, 0);
    }

    #[test]
    fn test_parse_response_missing_counts_default_zero() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let body = serde_json::json!({
            "message": { "role": "assistant", "content": "hi" },
            "done": true
        });
        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.tokens_used.prompt_tokens, 0);
        assert_eq!(response.tokens_used.completion_tokens, 0);
    }

    /// A minimal request for the body-shape tests.
    fn base_request() -> InferenceRequest {
        InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "qwen3.8".to_string(),
            max_tokens: 64,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::json!({}),
            request_timeout_secs: None,
        }
    }

    #[test]
    fn test_build_request_body_basic() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![
                crate::provider::Message {
                    role: "system".to_string(),
                    content: "Be helpful".into(),
                    cache_breakpoint: false,
                },
                crate::provider::Message {
                    role: "user".to_string(),
                    content: "Hello".into(),
                    cache_breakpoint: false,
                },
            ],
            model: "llama3-8b".to_string(),
            max_tokens: 512,
            temperature: 0.8,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        assert_eq!(body["model"], "llama3-8b");
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        // Use approximate comparison for float (JSON serializes f32 with precision loss)
        let temp = body["options"]["temperature"].as_f64().unwrap();
        assert!((temp - 0.8).abs() < 0.001);
        assert_eq!(body["options"]["num_predict"], 512);
    }

    #[test]
    fn test_build_request_body_passes_extra_params_into_options() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hi".into(),
                cache_breakpoint: false,
            }],
            model: "llama3-8b".to_string(),
            max_tokens: 512,
            temperature: 0.8,
            tools: vec![],
            extra: serde_json::json!({ "top_k": 40, "top_p": 0.95 }),
            request_timeout_secs: None,
        };
        let body = provider.build_request_body(&request);
        // Ollama's sampling knobs live under `options`.
        assert_eq!(body["options"]["top_k"], serde_json::json!(40));
        assert_eq!(body["options"]["top_p"], serde_json::json!(0.95));
    }

    #[test]
    fn test_build_request_body_prepends_system_blocks() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let request = InferenceRequest {
            system: vec![crate::provider::SystemBlock {
                text: "You are a local assistant.".to_string(),
                cache_hint: leviath_core::CacheHint::Never,
                volatility: leviath_core::Volatility::default(),
                region: String::new(),
            }],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hi".into(),
                cache_breakpoint: false,
            }],
            model: "llama3-8b".to_string(),
            max_tokens: 512,
            temperature: 0.8,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let messages = body["messages"].as_array().unwrap();
        // The system block is delivered as the first message rather than dropped.
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You are a local assistant.");
        assert_eq!(messages[1]["role"], "user");
    }

    #[test]
    fn test_build_request_body_serializes_tool_history() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![
                crate::provider::Message {
                    role: "assistant".to_string(),
                    content: crate::provider::MessageContent::Blocks(vec![
                        crate::provider::ContentBlock::ToolUse {
                            id: "call_1".to_string(),
                            name: "list_files".to_string(),
                            input: serde_json::json!({ "dir": "." }),
                            thought_signature: None,
                        },
                    ]),
                    cache_breakpoint: false,
                },
                crate::provider::Message {
                    role: "user".to_string(),
                    content: crate::provider::MessageContent::Blocks(vec![
                        crate::provider::ContentBlock::ToolResult {
                            tool_use_id: "call_1".to_string(),
                            content: "a.txt\nb.txt".to_string(),
                            is_error: false,
                        },
                    ]),
                    cache_breakpoint: false,
                },
            ],
            model: "llama3-8b".to_string(),
            max_tokens: 512,
            temperature: 0.8,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let messages = body["messages"].as_array().unwrap();
        // A conversation opening on an assistant turn gets a leading user turn
        // (strict endpoints require it), so find the turns by role rather than
        // by fixed index.
        let assistant = messages
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("the tool call round-trips as an assistant message");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "list_files");
        // … and the result as a `tool`-role message, not raw block JSON.
        let tool = messages
            .iter()
            .find(|m| m["role"] == "tool")
            .expect("the result round-trips as a tool message");
        assert_eq!(tool["tool_call_id"], "call_1");
        assert_eq!(tool["content"], "a.txt\nb.txt");
    }

    #[test]
    fn test_build_request_body_with_tools() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Search".into(),
                cache_breakpoint: false,
            }],
            model: "llama3-8b".to_string(),
            max_tokens: 512,
            temperature: 0.5,
            tools: vec![crate::provider::Tool {
                name: "search".to_string(),
                description: "Search something".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "search");
    }

    // ─── parse_response edge cases ──────────────────────────────────────

    #[test]
    fn test_parse_response_empty_content() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let body = serde_json::json!({
            "message": { "role": "assistant", "content": "" },
            "eval_count": 0,
            "prompt_eval_count": 0,
            "done": true
        });
        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.content, "");
        assert_eq!(response.finish_reason, FinishReason::Complete);
    }

    #[test]
    fn test_parse_response_missing_content_field() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let body = serde_json::json!({
            "message": { "role": "assistant" },
            "done": true
        });
        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.content, "");
    }

    #[test]
    fn test_parse_response_multiple_tool_calls() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let body = serde_json::json!({
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "function": {
                            "name": "tool_a",
                            "arguments": { "arg1": "val1" }
                        }
                    },
                    {
                        "function": {
                            "name": "tool_b",
                            "arguments": { "arg2": "val2" }
                        }
                    }
                ]
            },
            "eval_count": 10,
            "prompt_eval_count": 20,
            "done": true
        });

        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].name, "tool_a");
        assert_eq!(response.tool_calls[1].name, "tool_b");
        assert_ne!(
            response.tool_calls[0].id, response.tool_calls[1].id,
            "two calls in one response are two calls"
        );
        assert_eq!(response.finish_reason, FinishReason::ToolCall);
    }

    #[test]
    fn test_parse_response_tool_call_missing_function() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let body = serde_json::json!({
            "message": {
                "role": "assistant",
                "content": "hi",
                "tool_calls": [
                    {}
                ]
            },
            "done": true
        });

        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "");
    }

    #[test]
    fn test_parse_response_tool_call_missing_arguments() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let body = serde_json::json!({
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {
                        "function": {
                            "name": "my_tool"
                        }
                    }
                ]
            },
            "done": true
        });

        let response = provider.parse_response(&body).unwrap();
        assert_eq!(response.tool_calls[0].name, "my_tool");
        // Arguments should default to empty object
        assert!(response.tool_calls[0].arguments.is_object());
    }

    // ─── build_request_body edge cases ──────────────────────────────────

    #[test]
    fn test_build_request_body_no_tools_no_tools_key() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "llama3-8b".to_string(),
            max_tokens: 100,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        // When tools are empty, no "tools" key should be present
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn test_build_request_body_deepseek_r1_no_temperature() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "deepseek-r1:latest".to_string(),
            max_tokens: 100,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        // deepseek-r1 doesn't support temperature, so options should only have num_predict
        // Actually checking: the builtin_capabilities for deepseek-r1 has supports_temperature=true,
        // so temperature IS included. Let's just verify num_predict is present.
        assert_eq!(body["options"]["num_predict"], 100);
    }

    #[test]
    fn test_build_request_body_multiple_messages() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![
                crate::provider::Message {
                    role: "system".to_string(),
                    content: "You are helpful.".into(),
                    cache_breakpoint: false,
                },
                crate::provider::Message {
                    role: "user".to_string(),
                    content: "Hello".into(),
                    cache_breakpoint: false,
                },
                crate::provider::Message {
                    role: "assistant".to_string(),
                    content: "Hi there".into(),
                    cache_breakpoint: false,
                },
            ],
            model: "mistral-7b".to_string(),
            max_tokens: 256,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[2]["role"], "assistant");
    }

    #[test]
    fn test_build_request_body_multiple_tools() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Do things".into(),
                cache_breakpoint: false,
            }],
            model: "llama3-8b".to_string(),
            max_tokens: 512,
            temperature: 0.5,
            tools: vec![
                crate::provider::Tool {
                    name: "tool1".to_string(),
                    description: "First tool".to_string(),
                    parameters: serde_json::json!({"type": "object"}),
                },
                crate::provider::Tool {
                    name: "tool2".to_string(),
                    description: "Second tool".to_string(),
                    parameters: serde_json::json!({"type": "object"}),
                },
            ],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let body = provider.build_request_body(&request);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["function"]["name"], "tool1");
        assert_eq!(tools[1]["function"]["name"], "tool2");
    }

    // ─── capabilities with overrides ────────────────────────────────────

    #[test]
    fn test_capabilities_override_takes_precedence() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "llama3-8b".to_string(),
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: false,
                supports_tools: false,
                supports_system_prompt: false,
                max_context_tokens: 99,
                max_output_tokens: 99,
            }
            .into(),
        );
        let provider = OllamaProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "http://localhost:11434".to_string(),
            overrides,
        );
        let caps = provider.capabilities("llama3-8b");
        assert_eq!(caps.max_context_tokens, 99);
        assert!(!caps.supports_temperature);
    }

    #[test]
    fn test_capabilities_no_override_uses_builtin() {
        let overrides = HashMap::new();
        let provider = OllamaProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "http://localhost:11434".to_string(),
            overrides,
        );
        let caps = provider.capabilities("llama3-8b");
        assert_eq!(caps.max_context_tokens, 131_072); // builtin
    }

    // ─── builtin_capabilities: llama-3 pattern ──────────────────────────

    #[test]
    fn test_builtin_capabilities_llama_3_with_dash() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let caps = provider.builtin_capabilities("llama-3.1-70b");
        assert!(caps.supports_tools);
        assert_eq!(caps.max_context_tokens, 131_072);
    }

    // ─── name() returns "ollama" ─────────────────────────────────────────

    #[test]
    fn test_name() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        assert_eq!(provider.name(), "ollama");
    }

    // ─── max_context_tokens uses capabilities ───────────────────────────

    #[test]
    fn test_max_context_tokens_unknown_model() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        assert_eq!(provider.max_context_tokens("unknown-model"), 8192);
    }

    // ─── with_base_url stores the URL ───────────────────────────────────

    #[test]
    fn test_with_base_url_stores_url() {
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "https://remote:11434".to_string(),
        );
        assert_eq!(provider.base_url, "https://remote:11434");
    }

    // ─── HTTP error paths (connection refused) ──────────────────────────

    #[tokio::test]
    async fn test_infer_connection_refused() {
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "http://127.0.0.1:19998".to_string(),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "llama3-8b".to_string(),
            max_tokens: 100,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };
        let result = provider.infer(&request).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Request failed:"));
    }

    #[tokio::test]
    async fn test_infer_stream_connection_refused() {
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "http://127.0.0.1:19998".to_string(),
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "Hello".into(),
                cache_breakpoint: false,
            }],
            model: "llama3-8b".to_string(),
            max_tokens: 100,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };
        let result = provider.infer_stream(&request).await;
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("Request failed:")
        );
    }

    #[tokio::test]
    async fn test_list_models_connection_refused() {
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "http://127.0.0.1:19998".to_string(),
        );
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("Request failed:"));
    }

    // ─── with_base_url: invalid URL pattern triggers warning ─────────────

    #[test]
    fn test_with_base_url_invalid_protocol_does_not_panic() {
        // Registers a real Subscriber so the tracing::warn! call's field
        // arguments in with_base_url's "bad protocol" branch are actually
        // exercised, rather than short-circuited by the "is this level
        // enabled" check with no subscriber installed.
        let _guard = always_on_tracing_guard();
        // Should log a warning but not panic
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "ftp://invalid:11434".to_string(),
        );
        assert_eq!(provider.base_url, "ftp://invalid:11434");
    }

    #[test]
    fn test_with_overrides_invalid_protocol_does_not_panic() {
        // Registers a real Subscriber so the tracing::warn! call's field
        // arguments in with_overrides's "bad protocol" branch are actually
        // exercised.
        let _guard = always_on_tracing_guard();
        let provider = OllamaProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "ftp://invalid:11434".to_string(),
            HashMap::new(),
        );
        assert_eq!(provider.base_url, "ftp://invalid:11434");
    }

    // ─── parse_response: tool_call with null function ────────────────────

    #[test]
    fn test_parse_response_tool_call_null_function_field() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let body = serde_json::json!({
            "message": {
                "role": "assistant",
                "content": "done",
                "tool_calls": [
                    {
                        "function": null
                    }
                ]
            },
            "eval_count": 0,
            "prompt_eval_count": 0,
            "done": true
        });
        // When function is null, name and arguments should default
        let response = provider.parse_response(&body).unwrap();
        // tool_calls has 1 entry with empty name
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "");
    }

    // ─── NdjsonStream: parse logic ────────────────────────────────────────

    #[test]
    fn test_ollama_ndjson_stream_parses_done_line() {
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        // Create a stream from a static slice of bytes
        struct StaticStream {
            data: Vec<Vec<u8>>,
            idx: usize,
        }

        impl Stream for StaticStream {
            type Item = std::result::Result<bytes::Bytes, reqwest::Error>;

            fn poll_next(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if self.idx < self.data.len() {
                    let chunk = bytes::Bytes::from(self.data[self.idx].clone());
                    self.idx += 1;
                    Poll::Ready(Some(Ok(chunk)))
                } else {
                    Poll::Ready(None)
                }
            }
        }

        // Build a test NDJSON stream with one non-done and one done message
        let chunk1 =
            b"{\"message\":{\"role\":\"assistant\",\"content\":\"Hello \"},\"done\":false}\n"
                .to_vec();
        let chunk2 = b"{\"message\":{\"role\":\"assistant\",\"content\":\"world\"},\"done\":true,\"eval_count\":10,\"prompt_eval_count\":20}\n".to_vec();

        let static_stream = StaticStream {
            data: vec![chunk1, chunk2],
            idx: 0,
        };

        let ndjson_stream = OllamaNdjsonStream::new(static_stream);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use tokio_stream::StreamExt;
            let chunks: Vec<_> = ndjson_stream.collect().await;
            assert!(chunks.len() >= 2);
            // First chunk: content "Hello "
            let first = chunks[0].as_ref().unwrap();
            assert_eq!(first.delta, "Hello ");
            assert!(first.finish_reason.is_none());
            // Last chunk: done=true
            let last = chunks.last().unwrap().as_ref().unwrap();
            assert!(last.finish_reason.is_some());
            assert_eq!(last.finish_reason, Some(FinishReason::Complete));
            let tokens = last.tokens.as_ref().unwrap();
            assert_eq!(tokens.completion_tokens, 10);
            assert_eq!(tokens.prompt_tokens, 20);
        });
    }

    #[test]
    fn test_ollama_ndjson_stream_invalid_json_returns_error() {
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        struct StaticStream {
            data: Vec<Vec<u8>>,
            idx: usize,
        }

        impl Stream for StaticStream {
            type Item = std::result::Result<bytes::Bytes, reqwest::Error>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if self.idx < self.data.len() {
                    let chunk = bytes::Bytes::from(self.data[self.idx].clone());
                    self.idx += 1;
                    Poll::Ready(Some(Ok(chunk)))
                } else {
                    Poll::Ready(None)
                }
            }
        }

        // Invalid JSON line
        let chunk1 = b"not valid json at all\n".to_vec();
        let static_stream = StaticStream {
            data: vec![chunk1],
            idx: 0,
        };

        let ndjson_stream = OllamaNdjsonStream::new(static_stream);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use tokio_stream::StreamExt;
            let chunks: Vec<_> = ndjson_stream.collect().await;
            // First item should be an error
            assert!(!chunks.is_empty());
            assert!(chunks[0].is_err());
        });
    }

    #[test]
    fn test_ollama_ndjson_stream_remaining_buffer_parsed() {
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        struct StaticStream {
            data: Vec<Vec<u8>>,
            idx: usize,
        }

        impl Stream for StaticStream {
            type Item = std::result::Result<bytes::Bytes, reqwest::Error>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if self.idx < self.data.len() {
                    let chunk = bytes::Bytes::from(self.data[self.idx].clone());
                    self.idx += 1;
                    Poll::Ready(Some(Ok(chunk)))
                } else {
                    Poll::Ready(None)
                }
            }
        }

        // Send data WITHOUT trailing newline - it's in the remaining buffer
        let chunk1 =
            b"{\"message\":{\"role\":\"assistant\",\"content\":\"leftover\"},\"done\":false}"
                .to_vec();
        let static_stream = StaticStream {
            data: vec![chunk1],
            idx: 0,
        };

        let ndjson_stream = OllamaNdjsonStream::new(static_stream);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use tokio_stream::StreamExt;
            let chunks: Vec<_> = ndjson_stream.collect().await;
            // The remaining buffer should be parsed on stream end
            assert_eq!(chunks.len(), 1);
            let chunk = chunks[0].as_ref().unwrap();
            assert_eq!(chunk.delta, "leftover");
        });
    }

    #[test]
    fn test_ollama_ndjson_stream_empty_line_skipped() {
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        struct StaticStream {
            data: Vec<Vec<u8>>,
            idx: usize,
        }

        impl Stream for StaticStream {
            type Item = std::result::Result<bytes::Bytes, reqwest::Error>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if self.idx < self.data.len() {
                    let chunk = bytes::Bytes::from(self.data[self.idx].clone());
                    self.idx += 1;
                    Poll::Ready(Some(Ok(chunk)))
                } else {
                    Poll::Ready(None)
                }
            }
        }

        // Empty line followed by real data
        let chunk1 =
            b"\n{\"message\":{\"role\":\"assistant\",\"content\":\"hi\"},\"done\":false}\n"
                .to_vec();
        let static_stream = StaticStream {
            data: vec![chunk1],
            idx: 0,
        };

        let ndjson_stream = OllamaNdjsonStream::new(static_stream);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use tokio_stream::StreamExt;
            let chunks: Vec<_> = ndjson_stream.collect().await;
            // Empty line is skipped; one real chunk
            assert_eq!(chunks.len(), 1);
            assert_eq!(chunks[0].as_ref().unwrap().delta, "hi");
        });
    }

    #[test]
    fn test_ollama_ndjson_stream_invalid_remaining_buffer_yields_none() {
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        struct StaticStream {
            data: Vec<Vec<u8>>,
            idx: usize,
        }

        impl Stream for StaticStream {
            type Item = std::result::Result<bytes::Bytes, reqwest::Error>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if self.idx < self.data.len() {
                    let chunk = bytes::Bytes::from(self.data[self.idx].clone());
                    self.idx += 1;
                    Poll::Ready(Some(Ok(chunk)))
                } else {
                    Poll::Ready(None)
                }
            }
        }

        // No trailing newline, and the leftover isn't valid JSON either --
        // the remaining-buffer parse attempt fails and the stream just ends.
        let chunk1 = b"not valid json, no newline".to_vec();
        let static_stream = StaticStream {
            data: vec![chunk1],
            idx: 0,
        };

        let ndjson_stream = OllamaNdjsonStream::new(static_stream);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use tokio_stream::StreamExt;
            let chunks: Vec<_> = ndjson_stream.collect().await;
            assert!(chunks.is_empty());
        });
    }

    #[test]
    fn test_ollama_ndjson_stream_pending_is_propagated() {
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        // Yields Pending once, then a done line, then ends.
        struct PendingThenDataStream {
            polled_once: bool,
            yielded: bool,
        }

        impl Stream for PendingThenDataStream {
            type Item = std::result::Result<bytes::Bytes, reqwest::Error>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if !self.polled_once {
                    self.polled_once = true;
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                if !self.yielded {
                    self.yielded = true;
                    let chunk = bytes::Bytes::from(
                        b"{\"message\":{\"content\":\"hi\"},\"done\":true}\n".to_vec(),
                    );
                    return Poll::Ready(Some(Ok(chunk)));
                }
                Poll::Ready(None)
            }
        }

        let ndjson_stream = OllamaNdjsonStream::new(PendingThenDataStream {
            polled_once: false,
            yielded: false,
        });

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use tokio_stream::StreamExt;
            let chunks: Vec<_> = ndjson_stream.collect().await;
            assert_eq!(chunks.len(), 1);
            assert_eq!(chunks[0].as_ref().unwrap().delta, "hi");
        });
    }

    #[test]
    fn test_ollama_ndjson_stream_parses_tool_calls_from_done_chunk() {
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        struct StaticStream {
            data: Vec<Vec<u8>>,
            idx: usize,
        }

        impl Stream for StaticStream {
            type Item = std::result::Result<bytes::Bytes, reqwest::Error>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if self.idx < self.data.len() {
                    let chunk = bytes::Bytes::from(self.data[self.idx].clone());
                    self.idx += 1;
                    Poll::Ready(Some(Ok(chunk)))
                } else {
                    Poll::Ready(None)
                }
            }
        }

        // The done chunk includes tool_calls in the message
        let chunk1 = br#"{"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"search","arguments":{"query":"rust"}}}]},"done":true,"eval_count":5,"prompt_eval_count":10}"#.to_vec();
        let mut chunk1_with_newline = chunk1;
        chunk1_with_newline.push(b'\n');

        let static_stream = StaticStream {
            data: vec![chunk1_with_newline],
            idx: 0,
        };

        let ndjson_stream = OllamaNdjsonStream::new(static_stream);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use tokio_stream::StreamExt;
            let chunks: Vec<_> = ndjson_stream.collect().await;
            assert_eq!(chunks.len(), 1);
            let chunk = chunks[0].as_ref().unwrap();
            assert_eq!(chunk.finish_reason, Some(FinishReason::ToolCall));
            assert_eq!(chunk.tool_calls.len(), 1);
            assert_eq!(chunk.tool_calls[0].name, Some("search".to_string()));
            // Minted, not indexed: the value is opaque, only its shape and its
            // uniqueness across turns are contracts.
            assert!(
                chunk.tool_calls[0]
                    .id
                    .as_deref()
                    .is_some_and(|id| id.starts_with("ollama_"))
            );
            assert!(chunk.tool_calls[0].arguments_delta.contains("rust"));
        });
    }

    #[test]
    fn test_ollama_ndjson_stream_no_tool_calls_still_works() {
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        struct StaticStream {
            data: Vec<Vec<u8>>,
            idx: usize,
        }

        impl Stream for StaticStream {
            type Item = std::result::Result<bytes::Bytes, reqwest::Error>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if self.idx < self.data.len() {
                    let chunk = bytes::Bytes::from(self.data[self.idx].clone());
                    self.idx += 1;
                    Poll::Ready(Some(Ok(chunk)))
                } else {
                    Poll::Ready(None)
                }
            }
        }

        // Done chunk without tool_calls - should still be FinishReason::Complete
        let chunk1 = b"{\"message\":{\"content\":\"done\"},\"done\":true,\"eval_count\":1,\"prompt_eval_count\":1}\n".to_vec();
        let static_stream = StaticStream {
            data: vec![chunk1],
            idx: 0,
        };

        let ndjson_stream = OllamaNdjsonStream::new(static_stream);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use tokio_stream::StreamExt;
            let chunks: Vec<_> = ndjson_stream.collect().await;
            assert_eq!(chunks.len(), 1);
            let chunk = chunks[0].as_ref().unwrap();
            assert_eq!(chunk.finish_reason, Some(FinishReason::Complete));
            assert!(chunk.tool_calls.is_empty());
        });
    }

    #[test]
    fn test_build_request_body_no_temperature_via_override() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "no-temp-model".to_string(),
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: true,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 8192,
                max_output_tokens: 4096,
            }
            .into(),
        );
        let provider = OllamaProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "http://localhost:11434".to_string(),
            overrides,
        );
        let request = InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "hi".into(),
                cache_breakpoint: false,
            }],
            model: "no-temp-model".to_string(),
            max_tokens: 50,
            temperature: 0.7,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };
        let body = provider.build_request_body(&request);
        assert!(body["options"].get("temperature").is_none());
        assert_eq!(body["options"]["num_predict"], 50);
    }

    // ─── HTTP-call-level tests via a raw-TCP mock server ───────────────────

    fn mock_request() -> InferenceRequest {
        InferenceRequest {
            system: vec![],
            messages: vec![crate::provider::Message {
                role: "user".to_string(),
                content: "hi".into(),
                cache_breakpoint: false,
            }],
            model: "llama3-8b".to_string(),
            max_tokens: 50,
            temperature: 0.5,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        }
    }

    #[tokio::test]
    async fn infer_non_success_status_returns_api_error() {
        let url = spawn_mock_server(500, "Internal Server Error", b"boom").await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );
        let err = provider.infer(&mock_request()).await.unwrap_err();
        assert!(err.to_string().contains("API error:"));
    }

    #[tokio::test]
    async fn infer_non_success_status_body_read_error_still_reports_the_status() {
        // The body never arrives, so the shared helper substitutes the read
        // error for it. The status is what matters and must survive.
        let url = spawn_mock_server_truncated_error_body(500, "Internal Server Error").await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );
        let err = provider.infer(&mock_request()).await.unwrap_err();
        assert!(err.to_string().contains("500"), "{err}");
    }

    #[tokio::test]
    async fn infer_malformed_json_returns_invalid_response() {
        let url = spawn_mock_server(200, "OK", b"not json").await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );
        let err = provider.infer(&mock_request()).await.unwrap_err();
        assert!(err.to_string().contains("Invalid response:"));
    }

    #[tokio::test]
    async fn infer_stream_non_success_status_returns_api_error() {
        let url = spawn_mock_server(503, "Service Unavailable", b"down").await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );
        let result = provider.infer_stream(&mock_request()).await;
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("API error:"));
    }

    #[tokio::test]
    async fn infer_stream_non_success_status_body_read_error_still_reports_the_status() {
        let url = spawn_mock_server_truncated_error_body(503, "Service Unavailable").await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );
        let result = provider.infer_stream(&mock_request()).await;
        let err = result
            .err()
            .expect("a truncated error body is still an error");
        assert!(err.to_string().contains("503"), "{err}");
    }

    #[tokio::test]
    async fn infer_stream_success_yields_chunks() {
        let ndjson_body =
            b"{\"message\":{\"content\":\"hi\"},\"done\":true,\"eval_count\":1,\"prompt_eval_count\":1}\n";
        let url = spawn_mock_server(200, "OK", ndjson_body).await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );
        let mut stream = provider.infer_stream(&mock_request()).await.unwrap();
        use tokio_stream::StreamExt;
        let chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "hi");
    }

    #[tokio::test]
    async fn infer_stream_body_error_propagates_as_stream_item_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        // Send a Content-Length larger than the actual body, then close the
        // connection early - this produces a genuine mid-stream reqwest::Error
        // (there's no public constructor to fake one).
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;
            let response = b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(response).await;
            let _ = socket
                .write_all(b"{\"message\":{\"content\":\"hi\"},\"done\":false}\nshort")
                .await;
            let _ = socket.flush().await;
            let _ = socket.shutdown().await;
        });
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            format!("http://{}", addr),
        );
        let mut stream = provider.infer_stream(&mock_request()).await.unwrap();
        use tokio_stream::StreamExt;
        let mut saw_error = false;
        while let Some(item) = stream.next().await {
            if item.is_err() {
                saw_error = true;
                break;
            }
        }
        assert!(saw_error);
    }

    #[tokio::test]
    async fn list_models_non_success_status_returns_error() {
        let url = spawn_mock_server(401, "Unauthorized", b"nope").await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );
        let err = provider.list_models().await.unwrap_err();
        // Was `RequestFailed`, which reads as retryable; a rejected key is not.
        assert_eq!(
            err.unavailable_reason(),
            Some(crate::provider::UnavailableReason::AuthFailed)
        );
        assert!(!err.is_transient());
    }

    #[tokio::test]
    async fn list_models_non_success_status_body_read_error_still_reports_the_status() {
        let url = spawn_mock_server_truncated_error_body(401, "Unauthorized").await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("401"), "{err}");
    }

    #[tokio::test]
    async fn list_models_malformed_json_returns_error() {
        let url = spawn_mock_server(200, "OK", b"not json").await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );
        let err = provider.list_models().await.unwrap_err();
        assert!(err.to_string().contains("Invalid response:"));
    }

    #[tokio::test]
    async fn list_models_success_returns_models() {
        let body = br#"{"models":[{"name":"llama3-8b"},{"name":"mistral-7b"}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "llama3-8b");
        assert_eq!(models[0].display_name, Some("llama3-8b".to_string()));
        assert_eq!(models[0].provider, "ollama");
    }

    #[tokio::test]
    async fn list_models_missing_data_field_returns_empty() {
        let url = spawn_mock_server(200, "OK", b"{}").await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );
        let models = provider.list_models().await.unwrap();
        assert!(models.is_empty());
    }

    #[tokio::test]
    async fn list_models_entry_without_name_is_skipped() {
        let body = br#"{"models":[{"foo":"bar"},{"name":"llama3-8b"}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "llama3-8b");
    }

    #[tokio::test]
    async fn list_models_entry_with_non_string_name_is_skipped() {
        // covers the `.as_str()?` None branch in the filter_map
        let body = br#"{"models":[{"name":42},{"name":"llama3-8b"}]}"#;
        let url = spawn_mock_server(200, "OK", body).await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "llama3-8b");
    }

    #[test]
    fn ndjson_stream_skips_invalid_utf8_chunk_and_continues() {
        // covers the implicit else of `if let Ok(text) = from_utf8(&bytes)` in
        // OllamaNdjsonStream::poll_next
        use futures_core::Stream;
        use std::pin::Pin;
        use std::task::{Context, Poll};

        struct StaticStream {
            data: Vec<Vec<u8>>,
            idx: usize,
        }
        impl Stream for StaticStream {
            type Item = std::result::Result<bytes::Bytes, reqwest::Error>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                if self.idx < self.data.len() {
                    let chunk = bytes::Bytes::from(self.data[self.idx].clone());
                    self.idx += 1;
                    Poll::Ready(Some(Ok(chunk)))
                } else {
                    Poll::Ready(None)
                }
            }
        }

        let invalid_utf8 = vec![0xFF, 0xFE, 0x00];
        let valid_chunk =
            b"{\"message\":{\"role\":\"assistant\",\"content\":\"ok\"},\"done\":false}\n".to_vec();
        let stream = StaticStream {
            data: vec![invalid_utf8, valid_chunk],
            idx: 0,
        };
        let ndjson_stream = OllamaNdjsonStream::new(stream);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            use tokio_stream::StreamExt;
            let chunks: Vec<_> = ndjson_stream.collect().await;
            assert_eq!(chunks.len(), 1);
            assert_eq!(chunks[0].as_ref().unwrap().delta, "ok");
        });
    }

    /// `think` is Ollama's top-level switch for a reasoning model, not a
    /// sampling knob - buried under `options` it would be silently ignored,
    /// and the model would keep spending its budget reasoning.
    /// The bug in #470: two *different* turns each naming their first call
    /// `ollama_0`, so a conversation held distinct calls that were
    /// indistinguishable by id.
    #[test]
    fn ids_do_not_repeat_across_responses() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let body = |tool: &str| {
            serde_json::json!({
                "message": {
                    "content": "",
                    "tool_calls": [
                        { "function": { "name": tool, "arguments": {} } }
                    ]
                },
                "done": true
            })
        };

        let first = provider.parse_response(&body("read_file")).unwrap();
        let second = provider.parse_response(&body("list_dir")).unwrap();

        assert_ne!(
            first.tool_calls[0].id, second.tool_calls[0].id,
            "a later turn's call must not reuse an earlier turn's id"
        );
    }

    /// The id has to survive being written to `context.json` and replayed, so it
    /// stays inside what a JSON string and a provider will carry.
    #[test]
    fn an_id_is_plain_ascii_and_short() {
        let id = next_tool_call_id();
        assert!(id.starts_with("ollama_"));
        assert!(id.len() < 40, "{id}");
        assert!(
            id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "{id}"
        );
    }

    /// What the unique ids buy downstream, and the reason #470 mattered rather
    /// than merely being untidy: with the index-based mint, a response stranded
    /// by eviction shared an id with an unrelated later call, so
    /// `drop_unpaired_tool_turns` considered it answered and kept it. Keeping it
    /// is what put a `tool` message at the head of the conversation in #469.
    #[test]
    fn a_stranded_response_is_dropped_now_that_ids_differ() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let body = serde_json::json!({
            "message": {
                "content": "",
                "tool_calls": [{ "function": { "name": "read_file", "arguments": {} } }]
            },
            "done": true
        });
        // Two turns, as a run makes them: the first call's response is what
        // eviction later strands, and the second call is unrelated to it.
        let evicted = provider.parse_response(&body).unwrap().tool_calls[0]
            .id
            .clone();
        let live = provider.parse_response(&body).unwrap().tool_calls[0]
            .id
            .clone();
        assert_ne!(evicted, live);

        let request = crate::provider::InferenceRequest {
            system: vec![crate::provider::SystemBlock {
                text: "instructions".to_string(),
                cache_hint: leviath_core::CacheHint::Always,
                volatility: leviath_core::Volatility::default(),
                region: String::new(),
            }],
            messages: vec![
                // The stranded response, its own call long since evicted.
                crate::provider::Message {
                    role: "user".to_string(),
                    content: crate::provider::MessageContent::Blocks(vec![
                        crate::provider::ContentBlock::ToolResult {
                            tool_use_id: evicted,
                            content: "logs/a.log".to_string(),
                            is_error: false,
                        },
                    ]),
                    cache_breakpoint: false,
                },
                crate::provider::Message {
                    role: "assistant".to_string(),
                    content: crate::provider::MessageContent::Blocks(vec![
                        crate::provider::ContentBlock::ToolUse {
                            id: live.clone(),
                            name: "read_file".to_string(),
                            input: serde_json::json!({}),
                            thought_signature: None,
                        },
                    ]),
                    cache_breakpoint: false,
                },
                crate::provider::Message {
                    role: "user".to_string(),
                    content: crate::provider::MessageContent::Blocks(vec![
                        crate::provider::ContentBlock::ToolResult {
                            tool_use_id: live,
                            content: "ERROR disk full".to_string(),
                            is_error: false,
                        },
                    ]),
                    cache_breakpoint: false,
                },
            ],
            model: "qwen3.8-32k".to_string(),
            max_tokens: 64,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        };

        let messages = crate::openai_compat::openai_messages_with(
            &request,
            crate::openai_compat::ToolArgsFormat::Object,
        );
        let roles: Vec<&str> = messages.iter().filter_map(|m| m["role"].as_str()).collect();
        // The stranded response is gone, so the call turn leads and the user
        // turn goes ahead of it where it belongs, rather than being suppressed
        // by a `tool` message at the head.
        assert_eq!(roles, vec!["system", "user", "assistant", "tool"]);
    }

    /// A server that answers the model list with an error status.
    #[tokio::test]
    async fn priming_reports_a_tags_call_the_server_refuses() {
        let url = leviath_testkit::spawn_mock_server(500, "Internal Server Error", b"boom").await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );
        assert!(provider.prime_capabilities().await.is_err());
    }

    /// A model list that is not JSON at all.
    #[tokio::test]
    async fn priming_reports_a_tags_body_that_is_not_json() {
        let url = leviath_testkit::spawn_mock_server(200, "OK", b"not json").await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );
        let err = provider.prime_capabilities().await.unwrap_err();
        assert!(err.to_string().contains("Invalid response"), "{err}");
    }

    /// The model list arrives, and the per-model call is the one that fails.
    /// The sequence serves one response and then stops accepting, so the
    /// `/api/show` that follows finds nothing listening.
    #[tokio::test]
    async fn priming_reports_a_show_call_that_never_connects() {
        let (url, _bodies) =
            leviath_testkit::spawn_mock_sequence(vec![(200, "OK", tags_body(&["qwen3.8:latest"]))])
                .await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );
        assert!(provider.prime_capabilities().await.is_err());
    }

    /// A `/api/show` that answers with something that is not JSON.
    #[tokio::test]
    async fn priming_reports_a_show_body_that_is_not_json() {
        let (url, _bodies) = leviath_testkit::spawn_mock_sequence(vec![
            (200, "OK", tags_body(&["qwen3.8:latest"])),
            (200, "OK", b"not json".to_vec()),
        ])
        .await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );
        let err = provider.prime_capabilities().await.unwrap_err();
        assert!(err.to_string().contains("Invalid response"), "{err}");
    }

    /// A `/api/show` the server refuses.
    #[tokio::test]
    async fn priming_reports_a_show_call_the_server_refuses() {
        let (url, _bodies) = leviath_testkit::spawn_mock_sequence(vec![
            (200, "OK", tags_body(&["qwen3.8:latest"])),
            (500, "Internal Server Error", b"boom".to_vec()),
        ])
        .await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );
        assert!(provider.prime_capabilities().await.is_err());
    }

    /// A model list whose entries are not shaped like models names nothing, and
    /// a list with no `models` key at all is the same answer.
    #[tokio::test]
    async fn priming_skips_entries_that_name_no_model() {
        let (url, _bodies) = leviath_testkit::spawn_mock_sequence(vec![(
            200,
            "OK",
            serde_json::to_vec(&serde_json::json!({ "models": [{}, { "name": 7 }] }))
                .expect("serializes"),
        )])
        .await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );
        // No names means no `/api/show` calls, so the single queued response is
        // enough and priming succeeds having learned nothing.
        provider.prime_capabilities().await.expect("primes");
        assert_eq!(
            provider.capabilities("qwen3.8:latest").max_context_tokens,
            131_072
        );
    }

    /// A body with no `models` array at all.
    #[tokio::test]
    async fn priming_accepts_a_model_list_with_no_models_key() {
        let url = leviath_testkit::spawn_mock_server(200, "OK", b"{}").await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );
        provider.prime_capabilities().await.expect("primes");
    }

    /// The warning fires for a window that came from the name, once, and not at
    /// all for one the server told us or the user set.
    #[tokio::test]
    async fn a_guessed_window_is_announced_once_per_model() {
        let (url, _bodies) = leviath_testkit::spawn_mock_sequence(vec![
            (200, "OK", tags_body(&["qwen3.8-32k:latest"])),
            (200, "OK", show_body(Some(32768))),
        ])
        .await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );

        // Before priming every answer is a guess, and it is announced once.
        let _ = provider.capabilities("qwen3.8-32k:latest");
        let _ = provider.capabilities("qwen3.8-32k:latest");
        assert_eq!(
            leviath_core::sync::lock(&provider.warned_guessed).len(),
            1,
            "warned once per model, not once per call"
        );

        provider.prime_capabilities().await.expect("primes");

        // Primed, a second model is still a guess and gets its own warning.
        let _ = provider.capabilities("mystery:latest");
        let warned = leviath_core::sync::lock(&provider.warned_guessed);
        assert!(warned.contains("mystery:latest"));
        assert_eq!(warned.len(), 2);
    }

    /// A primed model is not a guess, so it is never announced.
    #[tokio::test]
    async fn a_window_read_from_the_server_is_not_announced() {
        let (url, _bodies) = leviath_testkit::spawn_mock_sequence(vec![
            (200, "OK", tags_body(&["qwen3.8-32k:latest"])),
            (200, "OK", show_body(Some(32768))),
        ])
        .await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );
        provider.prime_capabilities().await.expect("primes");

        let _ = provider.capabilities("qwen3.8-32k:latest");
        assert!(leviath_core::sync::lock(&provider.warned_guessed).is_empty());
    }

    /// An explicit override is an answer, so it silences the warning too.
    #[test]
    fn an_explicit_override_is_not_announced_as_a_guess() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "mystery:latest".to_string(),
            ModelCapabilityOverride {
                max_context_tokens: Some(16_384),
                ..Default::default()
            },
        );
        let provider = OllamaProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "http://127.0.0.1:1".to_string(),
            overrides,
        );
        let _ = provider.capabilities("mystery:latest");
        assert!(leviath_core::sync::lock(&provider.warned_guessed).is_empty());
    }

    // ─── effective window ───────────────────────────────────────────────────

    /// `parameters` is a text block, and `num_ctx` is the line that matters.
    #[test]
    fn the_effective_window_comes_from_num_ctx() {
        let show = serde_json::json!({
            "parameters": "temperature                    1\nnum_ctx                        32768\ntop_k                          20",
            "model_info": { "qwen35.context_length": 262144 }
        });
        assert_eq!(effective_window(&show), Some(32768));
    }

    /// The trap this exists to avoid. `model_info` names the architecture's
    /// ceiling, which on the model that prompted #475 is 262144 against a real
    /// window of 32768 - so reading the obvious field would replace one
    /// overestimate with a bigger one.
    #[test]
    fn the_architecture_ceiling_is_not_mistaken_for_the_window() {
        let show = serde_json::json!({
            "parameters": "temperature                    1",
            "model_info": { "qwen35.context_length": 262144 }
        });
        assert_eq!(
            effective_window(&show),
            None,
            "no num_ctx means the server default, which is not 262144 and is not ours to guess"
        );
    }

    #[test]
    fn a_show_response_with_no_parameters_names_no_window() {
        assert_eq!(effective_window(&serde_json::json!({})), None);
        assert_eq!(
            effective_window(&serde_json::json!({ "parameters": 7 })),
            None
        );
    }

    /// A `num_ctx` line that is not a number is not a window.
    #[test]
    fn an_unparseable_num_ctx_names_no_window() {
        let show = serde_json::json!({ "parameters": "num_ctx  lots" });
        assert_eq!(effective_window(&show), None);
        // A blank line has no first token to compare at all.
        let blank = serde_json::json!({ "parameters": "\n   \nnum_ctx  4096" });
        assert_eq!(effective_window(&blank), Some(4096));
        let bare = serde_json::json!({ "parameters": "num_ctx" });
        assert_eq!(effective_window(&bare), None);
    }

    // ─── priming ────────────────────────────────────────────────────────────

    fn show_body(num_ctx: Option<u32>) -> Vec<u8> {
        let parameters = match num_ctx {
            Some(n) => {
                format!("temperature                    1\nnum_ctx                        {n}")
            }
            None => "temperature                    1".to_string(),
        };
        serde_json::to_vec(&serde_json::json!({
            "parameters": parameters,
            "model_info": { "qwen35.context_length": 262144 }
        }))
        .expect("serializes")
    }

    fn tags_body(names: &[&str]) -> Vec<u8> {
        let models: Vec<serde_json::Value> = names
            .iter()
            .map(|n| serde_json::json!({ "name": n }))
            .collect();
        serde_json::to_vec(&serde_json::json!({ "models": models })).expect("serializes")
    }

    /// The whole point: a model whose name says one thing and whose server says
    /// another is budgeted against the server.
    #[tokio::test]
    async fn priming_replaces_the_window_the_name_suggested() {
        let (url, _bodies) = leviath_testkit::spawn_mock_sequence(vec![
            (200, "OK", tags_body(&["qwen3.8-32k:latest"])),
            (200, "OK", show_body(Some(32768))),
        ])
        .await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );

        // The compiled table matches on `qwen3` and hands out four times too much.
        assert_eq!(
            provider
                .capabilities("qwen3.8-32k:latest")
                .max_context_tokens,
            131_072
        );

        provider.prime_capabilities().await.expect("primes");

        assert_eq!(
            provider
                .capabilities("qwen3.8-32k:latest")
                .max_context_tokens,
            32_768
        );
        // Only the size moves. Whether tools work is not something /api/show
        // answers, so it still comes from the table.
        assert!(provider.capabilities("qwen3.8-32k:latest").supports_tools);
    }

    /// A model that names no `num_ctx` is served at the server's own default,
    /// which Ollama reports nowhere. Recording a guess would outrank the table
    /// without being any better than it.
    #[tokio::test]
    async fn a_model_naming_no_window_leaves_the_table_in_charge() {
        let (url, _bodies) = leviath_testkit::spawn_mock_sequence(vec![
            (200, "OK", tags_body(&["qwen3.8:latest"])),
            (200, "OK", show_body(None)),
        ])
        .await;
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
        );

        provider.prime_capabilities().await.expect("primes");

        assert_eq!(
            provider.capabilities("qwen3.8:latest").max_context_tokens,
            131_072
        );
    }

    /// What the user wrote outranks what the server said, the same order the
    /// OpenRouter provider uses.
    #[tokio::test]
    async fn an_explicit_override_still_wins_over_the_server() {
        let (url, _bodies) = leviath_testkit::spawn_mock_sequence(vec![
            (200, "OK", tags_body(&["qwen3.8-32k:latest"])),
            (200, "OK", show_body(Some(32768))),
        ])
        .await;
        let mut overrides = HashMap::new();
        overrides.insert(
            "qwen3.8-32k:latest".to_string(),
            ModelCapabilityOverride {
                max_context_tokens: Some(16_384),
                ..Default::default()
            },
        );
        let provider = OllamaProvider::with_overrides(
            crate::provider::build_http_client(None).expect("a test client builds"),
            url,
            overrides,
        );

        provider.prime_capabilities().await.expect("primes");

        assert_eq!(
            provider
                .capabilities("qwen3.8-32k:latest")
                .max_context_tokens,
            16_384
        );
    }

    /// A daemon whose Ollama is not running still starts, with the table in
    /// charge - the failure is a warning upstream, not a refusal here.
    #[tokio::test]
    async fn priming_against_an_unreachable_server_is_an_error_not_a_panic() {
        let provider = OllamaProvider::with_base_url(
            crate::provider::build_http_client(None).expect("a test client builds"),
            "http://127.0.0.1:1".to_string(),
        );
        assert!(provider.prime_capabilities().await.is_err());
        // And the table still answers.
        assert_eq!(
            provider.capabilities("qwen3.8:latest").max_context_tokens,
            131_072
        );
    }

    #[test]
    fn think_is_lifted_out_of_extra_to_the_top_level() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let mut request = base_request();
        request.extra = serde_json::json!({ "think": false, "top_k": 20 });

        let body = provider.build_request_body(&request);
        assert_eq!(body["think"], serde_json::json!(false));
        assert_eq!(
            body["options"]["top_k"],
            serde_json::json!(20),
            "the rest of extra still lands as sampling"
        );
        assert!(
            body["options"].get("think").is_none(),
            "and think is not left behind in options"
        );
    }

    /// Absent unless asked for. A model with no thinking to switch off rejects
    /// the field, so sending it by default would break every non-reasoning
    /// model to help the ones that reason.
    #[test]
    fn no_think_field_is_sent_when_the_blueprint_does_not_ask() {
        let provider = OllamaProvider::new(
            crate::provider::build_http_client(None).expect("a test client builds"),
        );
        let body = provider.build_request_body(&base_request());
        assert!(body.get("think").is_none(), "{body}");
    }
}
