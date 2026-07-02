//! Claude Code CLI provider.
//!
//! Uses the `claude` CLI (Claude Code) as an LLM backend, allowing users with a
//! Claude Code subscription to use Leviath without API keys.

use crate::provider::*;
use async_trait::async_trait;
use futures_core::Stream;
use std::collections::HashMap;
use std::pin::Pin;
use std::task::Poll;
use tokio::io::AsyncBufReadExt;

/// Provider that uses Claude Code CLI as the LLM backend.
///
/// This provider shells out to `claude --bare --print` for each inference call,
/// allowing users with a Claude Code subscription to use Leviath without API keys.
///
/// **Limitations compared to direct API providers:**
/// - No prompt caching (each call is a fresh process)
/// - Higher latency (~100-200ms process spawn overhead per call)
/// - Tool execution is handled by Claude Code internally, not by Leviath
/// - Tool result routing and per-stage tool filtering are not supported
/// - Not recommended for high-frequency tool loops (10+ iterations)
/// - Running many concurrent agents will be slower than API providers
pub struct ClaudeCodeProvider {
    /// Path to the claude binary (default: "claude")
    binary_path: String,
    /// Model capability overrides
    capability_overrides: HashMap<String, ModelCapabilities>,
}

impl ClaudeCodeProvider {
    /// Create a new provider with the default `claude` binary.
    pub fn new() -> Self {
        Self {
            binary_path: "claude".to_string(),
            capability_overrides: HashMap::new(),
        }
    }

    /// Create a new provider with a custom binary path.
    pub fn with_binary_path(path: String) -> Self {
        Self {
            binary_path: path,
            capability_overrides: HashMap::new(),
        }
    }

    /// Create a new provider with a custom binary path and capability overrides.
    pub fn with_overrides(
        binary: String,
        overrides: Option<HashMap<String, ModelCapabilities>>,
    ) -> Self {
        Self {
            binary_path: binary,
            capability_overrides: overrides.unwrap_or_default(),
        }
    }

    /// Built-in capabilities for known Claude models.
    fn builtin_capabilities(&self, model: &str) -> ModelCapabilities {
        // Claude Code doesn't expose temperature control and handles tools internally
        let (max_context, max_output) = if model.contains("opus") {
            (200_000, 32_000)
        } else if model.contains("haiku") {
            (200_000, 8_192)
        } else {
            // sonnet and other models
            (200_000, 16_000)
        };

        ModelCapabilities {
            supports_temperature: false,
            supports_streaming: true,
            supports_tools: false,
            supports_system_prompt: true,
            max_context_tokens: max_context,
            max_output_tokens: max_output,
        }
    }

    /// Build the user prompt from non-system messages.
    fn build_prompt(messages: &[Message]) -> String {
        let mut parts = Vec::new();
        for msg in messages {
            if msg.role == "system" {
                continue;
            }
            let role_label = match msg.role.as_str() {
                "user" => "User",
                "assistant" => "Assistant",
                other => other,
            };
            parts.push(format!("{}: {}", role_label, msg.content));
        }
        parts.join("\n")
    }

    /// Extract system prompt from messages.
    fn extract_system_prompt(messages: &[Message]) -> Option<String> {
        let system_parts: Vec<&str> = messages
            .iter()
            .filter(|m| m.role == "system")
            .map(|m| m.content.as_str())
            .collect();

        if system_parts.is_empty() {
            None
        } else {
            Some(system_parts.join("\n\n"))
        }
    }
}

/// Parse the JSON response from `claude --print --output-format json`.
///
/// Expected format:
/// ```json
/// {
///     "type": "result",
///     "subtype": "success",
///     "is_error": false,
///     "result": "The response text here",
///     "stop_reason": "end_turn",
///     "usage": { "input_tokens": 123, "output_tokens": 456 },
///     "total_cost_usd": 0.0
/// }
/// ```
fn parse_claude_response(output: &str) -> Result<InferenceResponse> {
    let json: serde_json::Value = serde_json::from_str(output).map_err(|e| {
        ProviderError::InvalidResponse(format!("Failed to parse Claude Code JSON response: {e}"))
    })?;

    parse_claude_json(&json)
}

/// Parse a Claude Code JSON value into an InferenceResponse.
fn parse_claude_json(json: &serde_json::Value) -> Result<InferenceResponse> {
    // Check for error responses
    if json
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let error_text = json
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error from Claude Code");
        return Err(ProviderError::ApiError(error_text.to_string()));
    }

    let content = json
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let prompt_tokens = json
        .pointer("/usage/input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;

    let completion_tokens = json
        .pointer("/usage/output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;

    let finish_reason = parse_stop_reason(json.get("stop_reason").and_then(|v| v.as_str()));

    Ok(InferenceResponse {
        content,
        tool_calls: Vec::new(),
        tokens_used: TokenUsage {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
            cached_tokens: 0,
            cache_write_tokens: 0,
        },
        finish_reason,
    })
}

/// Map a Claude stop_reason string to a FinishReason.
fn parse_stop_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("end_turn") | Some("stop") => FinishReason::Complete,
        Some("tool_use") => FinishReason::ToolCall,
        Some("max_tokens") => FinishReason::TokenLimit,
        _ => FinishReason::Complete,
    }
}

/// Parse an NDJSON line from stream-json output into an optional StreamChunk.
fn parse_stream_line(line: &str) -> Option<StreamChunk> {
    if line.trim().is_empty() {
        return None;
    }

    let json: serde_json::Value = serde_json::from_str(line).ok()?;
    let msg_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match msg_type {
        "assistant" => {
            // Content delta from assistant message
            let content = json.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if content.is_empty() {
                return None;
            }
            Some(StreamChunk {
                delta: content.to_string(),
                tool_calls: Vec::new(),
                tokens: None,
                finish_reason: None,
            })
        }
        "content_block_delta" => {
            // Alternative content delta format
            let delta = json.pointer("/delta/text").and_then(|v| v.as_str())?;
            if delta.is_empty() {
                return None;
            }
            Some(StreamChunk {
                delta: delta.to_string(),
                tool_calls: Vec::new(),
                tokens: None,
                finish_reason: None,
            })
        }
        "result" => {
            // Final result message with usage and stop_reason
            let content = json.get("result").and_then(|v| v.as_str()).unwrap_or("");
            let prompt_tokens = json
                .pointer("/usage/input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let completion_tokens = json
                .pointer("/usage/output_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;

            let finish_reason = parse_stop_reason(json.get("stop_reason").and_then(|v| v.as_str()));

            Some(StreamChunk {
                delta: content.to_string(),
                tool_calls: Vec::new(),
                tokens: Some(TokenUsage {
                    prompt_tokens,
                    completion_tokens,
                    total_tokens: prompt_tokens + completion_tokens,
                    cached_tokens: 0,
                    cache_write_tokens: 0,
                }),
                finish_reason: Some(finish_reason),
            })
        }
        _ => None,
    }
}

// Stream adapter that reads NDJSON lines from a Claude Code process and yields StreamChunks.
pin_project_lite::pin_project! {
    struct ClaudeCodeStream {
        #[pin]
        lines: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    }
}

impl Stream for ClaudeCodeStream {
    type Item = Result<StreamChunk>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = self.project();
        let lines = this.lines;

        // Poll the Lines future for the next line
        match lines.poll_next_line(cx) {
            Poll::Ready(Ok(Some(line))) => {
                if let Some(chunk) = parse_stream_line(&line) {
                    Poll::Ready(Some(Ok(chunk)))
                } else {
                    // Line didn't produce a chunk; wake to poll again
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
            Poll::Ready(Ok(None)) => Poll::Ready(None),
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(ProviderError::RequestFailed(format!(
                "Failed to read Claude Code output: {e}"
            ))))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl ClaudeCodeProvider {
    /// Core of [`Provider::infer`], with the process timeout injected so
    /// tests can exercise the timeout branch without a real 5-minute wait.
    async fn infer_with_timeout(
        &self,
        request: InferenceRequest,
        timeout_duration: std::time::Duration,
    ) -> Result<InferenceResponse> {
        let mut cmd = tokio::process::Command::new(&self.binary_path);
        cmd.args([
            "--bare",
            "--print",
            "--output-format",
            "json",
            "--no-session-persistence",
        ]);
        cmd.args(["--model", &request.model]);

        if let Some(system_prompt) = Self::extract_system_prompt(&request.messages) {
            cmd.args(["--system-prompt", &system_prompt]);
        }

        // Note: Claude Code handles tool execution internally; passing tool names
        // via --allowed-tools doesn't give Leviath control over tool results.
        if !request.tools.is_empty() {
            let tool_names: Vec<&str> = request.tools.iter().map(|t| t.name.as_str()).collect();
            cmd.args(["--allowed-tools", &tool_names.join(",")]);
        }

        let user_prompt = Self::build_prompt(&request.messages);
        cmd.arg(&user_prompt);

        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let child = cmd.spawn().map_err(|e| {
            ProviderError::RequestFailed(format!(
                "Failed to spawn '{}': {}. Is Claude Code installed?",
                self.binary_path, e
            ))
        })?;

        let output = tokio::time::timeout(timeout_duration, child.wait_with_output())
            .await
            .map_err(|_| {
                ProviderError::RequestFailed(format!(
                    "Claude Code process timed out after {}s",
                    timeout_duration.as_secs()
                ))
            })?
            .map_err(|e| {
                ProviderError::RequestFailed(format!("Claude Code process failed: {e}"))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ProviderError::RequestFailed(format!(
                "Claude Code exited with status {}: {}",
                output.status, stderr
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        parse_claude_response(&stdout)
    }
}

#[async_trait]
impl Provider for ClaudeCodeProvider {
    async fn infer(&self, request: InferenceRequest) -> Result<InferenceResponse> {
        self.infer_with_timeout(request, std::time::Duration::from_secs(300))
            .await
    }

    async fn infer_stream(
        &self,
        request: InferenceRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        let mut cmd = tokio::process::Command::new(&self.binary_path);
        cmd.args([
            "--bare",
            "--print",
            "--output-format",
            "stream-json",
            "--no-session-persistence",
        ]);
        cmd.args(["--model", &request.model]);

        if let Some(system_prompt) = Self::extract_system_prompt(&request.messages) {
            cmd.args(["--system-prompt", &system_prompt]);
        }

        if !request.tools.is_empty() {
            let tool_names: Vec<&str> = request.tools.iter().map(|t| t.name.as_str()).collect();
            cmd.args(["--allowed-tools", &tool_names.join(",")]);
        }

        let user_prompt = Self::build_prompt(&request.messages);
        cmd.arg(&user_prompt);

        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| {
            ProviderError::RequestFailed(format!(
                "Failed to spawn '{}': {}. Is Claude Code installed?",
                self.binary_path, e
            ))
        })?;

        let stdout = child.stdout.take().ok_or_else(|| {
            ProviderError::RequestFailed("Failed to capture Claude Code stdout".to_string())
        })?;

        let reader = tokio::io::BufReader::new(stdout);
        let lines = reader.lines();

        Ok(Box::pin(ClaudeCodeStream { lines }))
    }

    fn count_tokens(&self, text: &str, _model: &str) -> usize {
        // ~3.5 characters per token heuristic (same as Anthropic provider)
        (text.len() as f64 / 3.5).ceil() as usize
    }

    fn max_context_tokens(&self, model: &str) -> usize {
        if let Some(caps) = self.capability_overrides.get(model) {
            return caps.max_context_tokens;
        }
        // Most Claude models support 200k context
        200_000
    }

    fn name(&self) -> &str {
        "claude-code"
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        if let Some(caps) = self.capability_overrides.get(model) {
            return caps.clone();
        }
        self.builtin_capabilities(model)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let models = vec![
            ModelInfo {
                id: "claude-sonnet-4-6".to_string(),
                display_name: Some("Claude Sonnet 4.6".to_string()),
                provider: "claude-code".to_string(),
                capabilities: self.builtin_capabilities("claude-sonnet-4-6"),
            },
            ModelInfo {
                id: "claude-opus-4-8".to_string(),
                display_name: Some("Claude Opus 4.8".to_string()),
                provider: "claude-code".to_string(),
                capabilities: self.builtin_capabilities("claude-opus-4-8"),
            },
            ModelInfo {
                id: "claude-haiku-4-5".to_string(),
                display_name: Some("Claude Haiku 4.5".to_string()),
                provider: "claude-code".to_string(),
                capabilities: self.builtin_capabilities("claude-haiku-4-5"),
            },
        ];
        Ok(models)
    }
}

impl Default for ClaudeCodeProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_creates_default_binary_path() {
        let provider = ClaudeCodeProvider::new();
        assert_eq!(provider.binary_path, "claude");
    }

    #[test]
    fn test_with_binary_path_sets_custom_path() {
        let provider = ClaudeCodeProvider::with_binary_path("/usr/local/bin/claude".to_string());
        assert_eq!(provider.binary_path, "/usr/local/bin/claude");
    }

    #[test]
    fn test_with_overrides() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "custom-model".to_string(),
            ModelCapabilities {
                supports_temperature: true,
                supports_streaming: false,
                supports_tools: true,
                supports_system_prompt: true,
                max_context_tokens: 100_000,
                max_output_tokens: 8_000,
            },
        );
        let provider = ClaudeCodeProvider::with_overrides("claude".to_string(), Some(overrides));
        let caps = provider.capabilities("custom-model");
        assert!(caps.supports_temperature);
        assert!(!caps.supports_streaming);
        assert_eq!(caps.max_context_tokens, 100_000);
    }

    #[test]
    fn test_name() {
        let provider = ClaudeCodeProvider::new();
        assert_eq!(provider.name(), "claude-code");
    }

    #[test]
    fn test_capabilities_defaults() {
        let provider = ClaudeCodeProvider::new();
        let caps = provider.capabilities("claude-sonnet-4-6");
        assert!(!caps.supports_temperature);
        assert!(caps.supports_streaming);
        assert!(!caps.supports_tools);
        assert!(caps.supports_system_prompt);
        assert_eq!(caps.max_context_tokens, 200_000);
    }

    #[test]
    fn test_capabilities_opus() {
        let provider = ClaudeCodeProvider::new();
        let caps = provider.capabilities("claude-opus-4-8");
        assert_eq!(caps.max_output_tokens, 32_000);
    }

    #[test]
    fn test_capabilities_haiku() {
        let provider = ClaudeCodeProvider::new();
        let caps = provider.capabilities("claude-haiku-4-5");
        assert_eq!(caps.max_output_tokens, 8_192);
    }

    #[test]
    fn test_count_tokens() {
        let provider = ClaudeCodeProvider::new();
        let tokens = provider.count_tokens("Hello, world!", "claude-sonnet-4-6");
        assert!(tokens > 0);
        assert!(tokens < 100);
    }

    #[test]
    fn test_max_context_tokens() {
        let provider = ClaudeCodeProvider::new();
        assert_eq!(provider.max_context_tokens("claude-sonnet-4-6"), 200_000);
    }

    #[test]
    fn test_parse_successful_response() {
        let json = r#"{
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "Hello! How can I help you?",
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 42,
                "output_tokens": 15
            },
            "total_cost_usd": 0.001
        }"#;

        let response = parse_claude_response(json).unwrap();
        assert_eq!(response.content, "Hello! How can I help you?");
        assert_eq!(response.tokens_used.prompt_tokens, 42);
        assert_eq!(response.tokens_used.completion_tokens, 15);
        assert_eq!(response.tokens_used.total_tokens, 57);
        assert!(matches!(response.finish_reason, FinishReason::Complete));
        assert!(response.tool_calls.is_empty());
    }

    #[test]
    fn test_parse_error_response() {
        let json = r#"{
            "type": "result",
            "subtype": "error",
            "is_error": true,
            "result": "Rate limit exceeded",
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 0,
                "output_tokens": 0
            },
            "total_cost_usd": 0.0
        }"#;

        let result = parse_claude_response(json);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ProviderError::ApiError(_)));
        assert!(err.to_string().contains("Rate limit exceeded"));
    }

    #[test]
    fn test_parse_malformed_json() {
        let result = parse_claude_response("not valid json at all");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ProviderError::InvalidResponse(_)
        ));
    }

    #[test]
    fn test_parse_tool_use_stop_reason() {
        let json = r#"{
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "Let me search for that.",
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 10, "output_tokens": 8 },
            "total_cost_usd": 0.0
        }"#;

        let response = parse_claude_response(json).unwrap();
        assert!(matches!(response.finish_reason, FinishReason::ToolCall));
    }

    #[test]
    fn test_parse_max_tokens_stop_reason() {
        let json = r#"{
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "This response was truncated because",
            "stop_reason": "max_tokens",
            "usage": { "input_tokens": 100, "output_tokens": 4096 },
            "total_cost_usd": 0.0
        }"#;

        let response = parse_claude_response(json).unwrap();
        assert!(matches!(response.finish_reason, FinishReason::TokenLimit));
    }

    #[test]
    fn test_parse_stream_line_assistant() {
        let line = r#"{"type":"assistant","content":"Hello there"}"#;
        let chunk = parse_stream_line(line).unwrap();
        assert_eq!(chunk.delta, "Hello there");
        assert!(chunk.tokens.is_none());
        assert!(chunk.finish_reason.is_none());
    }

    #[test]
    fn test_parse_stream_line_content_block_delta() {
        let line = r#"{"type":"content_block_delta","delta":{"text":"world"}}"#;
        let chunk = parse_stream_line(line).unwrap();
        assert_eq!(chunk.delta, "world");
    }

    #[test]
    fn test_parse_stream_line_result() {
        let line = r#"{"type":"result","result":"Done","stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":5}}"#;
        let chunk = parse_stream_line(line).unwrap();
        assert_eq!(chunk.delta, "Done");
        assert!(chunk.tokens.is_some());
        let tokens = chunk.tokens.unwrap();
        assert_eq!(tokens.prompt_tokens, 10);
        assert_eq!(tokens.completion_tokens, 5);
        assert!(matches!(chunk.finish_reason, Some(FinishReason::Complete)));
    }

    #[test]
    fn test_parse_stream_line_unknown_type() {
        let line = r#"{"type":"ping"}"#;
        assert!(parse_stream_line(line).is_none());
    }

    #[test]
    fn test_parse_stream_line_empty() {
        assert!(parse_stream_line("").is_none());
        assert!(parse_stream_line("   ").is_none());
    }

    #[test]
    fn test_parse_stream_line_invalid_json() {
        assert!(parse_stream_line("not json").is_none());
    }

    #[test]
    fn test_build_prompt() {
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "You are helpful.".to_string(),
                cache_breakpoint: false,
            },
            Message {
                role: "user".to_string(),
                content: "Hello".to_string(),
                cache_breakpoint: false,
            },
            Message {
                role: "assistant".to_string(),
                content: "Hi there!".to_string(),
                cache_breakpoint: false,
            },
            Message {
                role: "user".to_string(),
                content: "How are you?".to_string(),
                cache_breakpoint: false,
            },
        ];

        let prompt = ClaudeCodeProvider::build_prompt(&messages);
        assert!(!prompt.contains("system"));
        assert!(prompt.contains("User: Hello"));
        assert!(prompt.contains("Assistant: Hi there!"));
        assert!(prompt.contains("User: How are you?"));
    }

    #[test]
    fn test_extract_system_prompt() {
        let messages = vec![
            Message {
                role: "system".to_string(),
                content: "You are helpful.".to_string(),
                cache_breakpoint: false,
            },
            Message {
                role: "system".to_string(),
                content: "Be concise.".to_string(),
                cache_breakpoint: false,
            },
            Message {
                role: "user".to_string(),
                content: "Hello".to_string(),
                cache_breakpoint: false,
            },
        ];

        let system = ClaudeCodeProvider::extract_system_prompt(&messages).unwrap();
        assert!(system.contains("You are helpful."));
        assert!(system.contains("Be concise."));
    }

    #[test]
    fn test_extract_system_prompt_none() {
        let messages = vec![Message {
            role: "user".to_string(),
            content: "Hello".to_string(),
            cache_breakpoint: false,
        }];

        assert!(ClaudeCodeProvider::extract_system_prompt(&messages).is_none());
    }

    #[test]
    fn test_default_impl() {
        let provider = ClaudeCodeProvider::default();
        assert_eq!(provider.binary_path, "claude");
        assert_eq!(provider.name(), "claude-code");
    }

    // ── Additional coverage tests ──────────────────────────────────────────

    #[test]
    fn test_with_overrides_none() {
        let provider = ClaudeCodeProvider::with_overrides("claude".to_string(), None);
        assert!(provider.capability_overrides.is_empty());
    }

    #[test]
    fn test_max_context_tokens_with_override() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "custom-model".to_string(),
            ModelCapabilities {
                supports_temperature: false,
                supports_streaming: false,
                supports_tools: false,
                supports_system_prompt: false,
                max_context_tokens: 50_000,
                max_output_tokens: 5_000,
            },
        );
        let provider = ClaudeCodeProvider::with_overrides("claude".to_string(), Some(overrides));
        assert_eq!(provider.max_context_tokens("custom-model"), 50_000);
    }

    #[test]
    fn test_max_context_tokens_default() {
        let provider = ClaudeCodeProvider::new();
        assert_eq!(provider.max_context_tokens("some-unknown"), 200_000);
    }

    #[test]
    fn test_builtin_capabilities_sonnet_default() {
        let provider = ClaudeCodeProvider::new();
        let caps = provider.builtin_capabilities("claude-sonnet-4-6");
        assert_eq!(caps.max_output_tokens, 16_000);
    }

    #[test]
    fn test_count_tokens_larger_text() {
        let provider = ClaudeCodeProvider::new();
        let text = "a".repeat(350);
        let tokens = provider.count_tokens(&text, "claude-sonnet-4-6");
        assert_eq!(tokens, 100); // ceil(350 / 3.5) = 100
    }

    #[test]
    fn test_count_tokens_empty() {
        let provider = ClaudeCodeProvider::new();
        let tokens = provider.count_tokens("", "claude-sonnet-4-6");
        assert_eq!(tokens, 0);
    }

    #[test]
    fn test_parse_stop_reason_variants() {
        assert!(matches!(
            parse_stop_reason(Some("end_turn")),
            FinishReason::Complete
        ));
        assert!(matches!(
            parse_stop_reason(Some("stop")),
            FinishReason::Complete
        ));
        assert!(matches!(
            parse_stop_reason(Some("tool_use")),
            FinishReason::ToolCall
        ));
        assert!(matches!(
            parse_stop_reason(Some("max_tokens")),
            FinishReason::TokenLimit
        ));
        assert!(matches!(parse_stop_reason(None), FinishReason::Complete));
        assert!(matches!(
            parse_stop_reason(Some("unknown")),
            FinishReason::Complete
        ));
    }

    #[test]
    fn test_parse_claude_response_missing_fields() {
        let json = r#"{"type":"result","subtype":"success","is_error":false}"#;
        let response = parse_claude_response(json).unwrap();
        assert_eq!(response.content, "");
        assert_eq!(response.tokens_used.prompt_tokens, 0);
        assert_eq!(response.tokens_used.completion_tokens, 0);
        assert!(matches!(response.finish_reason, FinishReason::Complete));
    }

    #[test]
    fn test_parse_stream_line_assistant_empty_content() {
        let line = r#"{"type":"assistant","content":""}"#;
        assert!(parse_stream_line(line).is_none());
    }

    #[test]
    fn test_parse_stream_line_content_block_delta_empty() {
        let line = r#"{"type":"content_block_delta","delta":{"text":""}}"#;
        assert!(parse_stream_line(line).is_none());
    }

    #[test]
    fn test_parse_stream_line_result_with_tool_use_stop() {
        let line = r#"{"type":"result","result":"","stop_reason":"tool_use","usage":{"input_tokens":5,"output_tokens":3}}"#;
        let chunk = parse_stream_line(line).unwrap();
        assert!(matches!(chunk.finish_reason, Some(FinishReason::ToolCall)));
    }

    #[test]
    fn test_build_prompt_unknown_role() {
        let messages = vec![Message {
            role: "tool".to_string(),
            content: "result data".to_string(),
            cache_breakpoint: false,
        }];
        let prompt = ClaudeCodeProvider::build_prompt(&messages);
        assert!(prompt.contains("tool: result data"));
    }

    #[tokio::test]
    async fn test_list_models() {
        let provider = ClaudeCodeProvider::new();
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 3);
        assert!(models.iter().any(|m| m.id == "claude-sonnet-4-6"));
        assert!(models.iter().any(|m| m.id == "claude-opus-4-8"));
        assert!(models.iter().any(|m| m.id == "claude-haiku-4-5"));
        for model in &models {
            assert_eq!(model.provider, "claude-code");
        }
    }

    // ─── infer()/infer_stream(): stub `claude` binary via with_binary_path ──
    //
    // ClaudeCodeProvider shells out to a real subprocess, so exercising
    // infer()/infer_stream() means substituting a fake "claude" binary — a
    // small shell script that ignores its args and prints canned output —
    // via the existing `with_binary_path` test seam.
    //
    // This whole approach (a `#!/bin/sh` shebang script, `chmod +x`'d and
    // spawned directly) is Unix-only: Windows' `CreateProcess` doesn't
    // understand shebangs and can't execute a `.sh` file as a native binary
    // at all -- every test using this failed on Windows CI with "%1 is not
    // a valid Win32 application" (os error 193). `write_stub_script` itself
    // and every test that calls it are therefore `#[cfg(unix)]`; the two
    // spawn-failure tests below (which point at a nonexistent path rather
    // than a real script) remain cross-platform since a missing file fails
    // to spawn identically on every OS.
    #[cfg(unix)]
    fn write_stub_script(tag: &str, body: &str) -> std::path::PathBuf {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!(
            "lev-claude-stub-{}-{}.sh",
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        f.write_all(body.as_bytes()).unwrap();
        drop(f);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn make_request() -> InferenceRequest {
        InferenceRequest {
            messages: vec![Message {
                role: "user".to_string(),
                content: "hi".to_string(),
                cache_breakpoint: false,
            }],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn infer_success_parses_response() {
        let script = write_stub_script(
            "infer-ok",
            "echo '{\"result\": \"hello from stub\", \"usage\": {\"input_tokens\": 3, \"output_tokens\": 2}}'\n",
        );
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        let resp = provider.infer(make_request()).await.unwrap();
        assert_eq!(resp.content, "hello from stub");
        let _ = std::fs::remove_file(&script);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn infer_with_timeout_fires_on_slow_process() {
        // A stub that outlives a short injected timeout, exercising the
        // real `tokio::time::timeout` branch in `infer_with_timeout` --
        // `infer()` itself hardcodes a real 5-minute timeout, far too long
        // to wait for in a test.
        let script = write_stub_script("infer-slow", "sleep 5\necho '{\"result\": \"late\"}'\n");
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        let err = provider
            .infer_with_timeout(make_request(), std::time::Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::RequestFailed(_)));
        assert!(err.to_string().contains("timed out"));
        let _ = std::fs::remove_file(&script);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn infer_is_error_response_returns_api_error() {
        let script = write_stub_script(
            "infer-err",
            "echo '{\"is_error\": true, \"result\": \"bad request\"}'\n",
        );
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        let err = provider.infer(make_request()).await.unwrap_err();
        assert!(matches!(err, ProviderError::ApiError(_)));
        assert!(err.to_string().contains("bad request"));
        let _ = std::fs::remove_file(&script);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn infer_nonzero_exit_returns_request_failed() {
        let script = write_stub_script("infer-fail", "echo 'boom' >&2\nexit 1\n");
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        let err = provider.infer(make_request()).await.unwrap_err();
        assert!(matches!(err, ProviderError::RequestFailed(_)));
        assert!(err.to_string().contains("boom"));
        let _ = std::fs::remove_file(&script);
    }

    #[tokio::test]
    async fn infer_spawn_failure_returns_request_failed() {
        let provider = ClaudeCodeProvider::with_binary_path(
            "/nonexistent/definitely/not/a/real/binary".to_string(),
        );
        let err = provider.infer(make_request()).await.unwrap_err();
        assert!(matches!(err, ProviderError::RequestFailed(_)));
        assert!(err.to_string().contains("Is Claude Code installed?"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn infer_with_system_prompt_and_tools_still_succeeds() {
        let script = write_stub_script("infer-tools", "echo '{\"result\": \"ok\"}'\n");
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        let mut req = make_request();
        req.messages.insert(
            0,
            Message {
                role: "system".to_string(),
                content: "be nice".to_string(),
                cache_breakpoint: false,
            },
        );
        req.tools = vec![Tool {
            name: "bash".to_string(),
            description: "run bash".to_string(),
            parameters: serde_json::json!({}),
        }];
        let resp = provider.infer(req).await.unwrap();
        assert_eq!(resp.content, "ok");
        let _ = std::fs::remove_file(&script);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn infer_stream_yields_chunks_from_ndjson() {
        let script = write_stub_script(
            "stream-ok",
            "echo '{\"type\": \"assistant\", \"content\": \"Hello\"}'\n\
             echo '{\"type\": \"assistant\", \"content\": \" world\"}'\n",
        );
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        let mut stream = provider.infer_stream(make_request()).await.unwrap();
        use tokio_stream::StreamExt;
        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(first.delta, "Hello");
        let second = stream.next().await.unwrap().unwrap();
        assert_eq!(second.delta, " world");
        assert!(stream.next().await.is_none());
        let _ = std::fs::remove_file(&script);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn infer_stream_with_system_prompt_and_tools_still_succeeds() {
        let script = write_stub_script(
            "stream-tools",
            "echo '{\"type\": \"assistant\", \"content\": \"ok\"}'\n",
        );
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        let mut req = make_request();
        req.messages.insert(
            0,
            Message {
                role: "system".to_string(),
                content: "be nice".to_string(),
                cache_breakpoint: false,
            },
        );
        req.tools = vec![Tool {
            name: "bash".to_string(),
            description: "run bash".to_string(),
            parameters: serde_json::json!({}),
        }];
        let mut stream = provider.infer_stream(req).await.unwrap();
        use tokio_stream::StreamExt;
        let chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "ok");
        let _ = std::fs::remove_file(&script);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn infer_stream_content_block_delta_variant() {
        let script = write_stub_script(
            "stream-cbd",
            "echo '{\"type\": \"content_block_delta\", \"delta\": {\"text\": \"partial\"}}'\n",
        );
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        let mut stream = provider.infer_stream(make_request()).await.unwrap();
        use tokio_stream::StreamExt;
        let chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "partial");
        let _ = std::fs::remove_file(&script);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn infer_stream_skips_blank_and_unparseable_lines() {
        let script = write_stub_script(
            "stream-skip",
            "echo ''\n\
             echo 'not json'\n\
             echo '{\"type\": \"assistant\", \"content\": \"real\"}'\n",
        );
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        let mut stream = provider.infer_stream(make_request()).await.unwrap();
        use tokio_stream::StreamExt;
        let chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "real");
        let _ = std::fs::remove_file(&script);
    }

    #[tokio::test]
    async fn infer_stream_spawn_failure_returns_error() {
        let provider = ClaudeCodeProvider::with_binary_path(
            "/nonexistent/definitely/not/a/real/binary".to_string(),
        );
        match provider.infer_stream(make_request()).await {
            Err(ProviderError::RequestFailed(_)) => {}
            _ => panic!("expected RequestFailed"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn infer_stream_invalid_utf8_output_yields_read_error() {
        // tokio's `Lines::poll_next_line` requires valid UTF-8 (like
        // `std::io::BufRead::read_line`) -- a raw invalid byte sequence on
        // stdout surfaces as a genuine `io::Error`, exercising
        // `ClaudeCodeStream::poll_next`'s `Poll::Ready(Err(e))` arm.
        //
        // Uses octal (`\NNN`) rather than hex (`\xHH`) escapes: `\xHH` is a
        // bash/ksh printf extension, not POSIX -- Debian/Ubuntu's `/bin/sh`
        // (dash) doesn't support it and this script's shebang is `#!/bin/sh`,
        // so on Ubuntu CI `printf` wrote something other than the intended
        // invalid bytes and the stream produced no output at all instead of
        // a read error. `\NNN` octal escapes are POSIX-standard printf and
        // portable across dash/bash/zsh alike. 0377=0xff, 0376=0xfe.
        let script = write_stub_script("stream-badutf8", "printf '\\377\\376\\n'\n");
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        let mut stream = provider.infer_stream(make_request()).await.unwrap();
        use tokio_stream::StreamExt;
        let err = stream
            .next()
            .await
            .expect("stream should yield an item")
            .expect_err("expected a read error from invalid UTF-8");
        assert!(matches!(err, ProviderError::RequestFailed(_)));
        assert!(err
            .to_string()
            .contains("Failed to read Claude Code output"));
        let _ = std::fs::remove_file(&script);
    }
}
