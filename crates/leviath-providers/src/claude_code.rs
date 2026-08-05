//! Claude Code CLI provider.
//!
//! Uses the `claude` CLI as a plain inference transport so a user with a Claude
//! subscription can run Leviath without an API key.
//!
//! The CLI is driven as a *relay*, not as an agent: its own tools, settings
//! files, MCP servers and slash commands are all switched off, Leviath's
//! assembled system blocks become the entire system prompt, and tool calling
//! rides the text protocol in [`crate::text_tools`]. Leviath keeps ownership of
//! the context window, the tool loop, and the iteration count.
//!
//! **Flags that matter, and why:**
//! - No `--bare`. It looks like the right lockdown switch, but it documents
//!   itself as "Anthropic auth is strictly ANTHROPIC_API_KEY or apiKeyHelper -
//!   OAuth and keychain are never read", so under a subscription every call
//!   fails with `Not logged in · Please run /login`. Every mechanism that
//!   suppresses the CLI's injected context also disables OAuth; the two are
//!   mutually exclusive.
//! - No `--allowed-tools`. Leviath tool names passed there would ask *Claude
//!   Code* to run *its own* tools under those names, and the results would never
//!   reach Leviath.
//! - `--system-prompt-file` rather than `--system-prompt`: an assembled context
//!   can be hundreds of kilobytes, well past Linux's 128 KB per-argument limit.
//!   The prompt goes on stdin for the same reason.
//! - `--effort` is always passed explicitly. Left alone the CLI picks `high`
//!   with adaptive thinking, spending output tokens and latency Leviath never
//!   asked for.
//!
//! **Limitations compared to a direct API provider:**
//! - The CLI adds ~130 tokens of its own context to every call - a billing
//!   header, an identity line, the current date, and (on the OAuth path) the
//!   user's account email address. None of it can be disabled.
//! - No prompt caching: each call is a fresh process with a fresh session.
//! - ~200 ms process-spawn overhead per inference.
//! - Anthropic models only, and the CLI ignores the request's `max_tokens` and
//!   `temperature` in favour of its own.

use crate::provider::*;
use crate::text_tools;
use async_trait::async_trait;
use std::collections::HashMap;
use std::io::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::AsyncWriteExt as _;

/// Reasoning effort passed to the CLI when the caller configures none.
///
/// The CLI's own default is `high` with adaptive thinking. Leviath drives its
/// own multi-stage loop and picks a model per stage, so paying for maximum
/// per-call deliberation on top of that is waste; `medium` is the balance point.
pub const DEFAULT_EFFORT: &str = "medium";

/// Effort levels the CLI accepts.
pub const EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Tokens held back from the advertised context window to cover the context the
/// CLI injects on every call that we can neither see nor disable. Measured at
/// ~130 tokens for the floor; the margin covers growth across CLI versions so
/// region budgets are never quietly tighter than Leviath believes.
const INJECTION_RESERVE_TOKENS: usize = 2_000;

/// Provider that uses the Claude Code CLI as an inference transport.
pub struct ClaudeCodeProvider {
    /// Path to the claude binary (default: "claude")
    binary_path: String,
    /// Reasoning effort passed as `--effort`.
    effort: String,
    /// Model capability overrides
    capability_overrides: HashMap<String, ModelCapabilities>,
    /// Source of tool-call ids. The CLI gives us no ids of its own, and ids must
    /// stay unique for the life of a transcript, so they are handed out
    /// monotonically rather than restarting at zero each response.
    next_call_id: AtomicU64,
}

impl ClaudeCodeProvider {
    /// Create a new provider with the default `claude` binary and effort.
    pub fn new() -> Self {
        Self::with_overrides("claude".to_string(), None, None)
    }

    /// Create a new provider with a custom binary path.
    pub fn with_binary_path(path: String) -> Self {
        Self::with_overrides(path, None, None)
    }

    /// Create a new provider with a custom binary path, effort level, and
    /// capability overrides.
    ///
    /// An `effort` that isn't one of [`EFFORT_LEVELS`] falls back to
    /// [`DEFAULT_EFFORT`] rather than being passed through to the CLI, which
    /// would reject it and fail every call.
    pub fn with_overrides(
        binary: String,
        effort: Option<String>,
        overrides: Option<HashMap<String, ModelCapabilities>>,
    ) -> Self {
        let effort = effort
            .filter(|e| EFFORT_LEVELS.contains(&e.as_str()))
            .unwrap_or_else(|| DEFAULT_EFFORT.to_string());
        Self {
            binary_path: binary,
            effort,
            capability_overrides: overrides.unwrap_or_default(),
            next_call_id: AtomicU64::new(1),
        }
    }

    /// Built-in capabilities for known Claude models.
    fn builtin_capabilities(&self, model: &str) -> ModelCapabilities {
        let max_output = if model.contains("opus") {
            32_000
        } else if model.contains("haiku") {
            8_192
        } else {
            16_000
        };

        ModelCapabilities {
            // The CLI exposes no temperature control.
            supports_temperature: false,
            // Streaming is not implemented: the runtime never calls
            // `infer_stream`, and the trait's default wraps `infer`.
            supports_streaming: false,
            // Synthesized by the text protocol rather than native.
            supports_tools: true,
            supports_system_prompt: true,
            max_context_tokens: 200_000 - INJECTION_RESERVE_TOKENS,
            max_output_tokens: max_output,
        }
    }

    /// The full system prompt: Leviath's assembled system blocks, plus the tool
    /// catalog and protocol when the stage has tools.
    ///
    /// This is the fix for the transport's central bug - `ContextWindow::assemble`
    /// puts every structured region except the sliding message window into
    /// `request.system`, and the previous implementation read only messages with
    /// `role == "system"`, a field `assemble()` never populates. The regions were
    /// dropped on the floor.
    fn build_system_prompt(request: &InferenceRequest) -> String {
        let mut parts: Vec<String> = request
            .system
            .iter()
            .map(|b| b.text.clone())
            .filter(|t| !t.is_empty())
            .collect();

        let suffix = text_tools::render_system_suffix(&request.tools);
        if !suffix.is_empty() {
            parts.push(suffix);
        }
        parts.join("\n\n")
    }

    /// Assign runtime-owned ids to the calls parsed out of a reply.
    fn assign_ids(&self, calls: Vec<(String, serde_json::Value)>) -> Vec<ToolCall> {
        calls
            .into_iter()
            .map(|(name, arguments)| ToolCall {
                id: format!(
                    "cc_call_{}",
                    self.next_call_id.fetch_add(1, Ordering::Relaxed)
                ),
                name,
                arguments,
                thought_signature: None,
            })
            .collect()
    }

    /// Core of [`Provider::infer`], with the process timeout and the temp
    /// directory injected so tests can exercise the timeout and prompt-staging
    /// failure branches without a real 5-minute wait or a full disk.
    async fn infer_with_timeout(
        &self,
        request: &InferenceRequest,
        temp_dir: &std::path::Path,
        timeout_duration: std::time::Duration,
    ) -> Result<InferenceResponse> {
        // The system prompt goes via a file (it routinely exceeds Linux's 128 KB
        // cap on a single argv entry), and the transcript via stdin (same reason).
        let prompt_file = stage_prompt_file(temp_dir, &Self::build_system_prompt(request))
            .map_err(prompt_file_error)?;

        let mut cmd = tokio::process::Command::new(&self.binary_path);
        cmd.args([
            "--print",
            "--output-format",
            "json",
            "--no-session-persistence",
            // Lock the CLI down to a relay: no built-in tools, no settings
            // files or CLAUDE.md, no ambient MCP servers, no skills.
            "--tools",
            "",
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--disable-slash-commands",
        ]);
        cmd.args(["--model", &request.model]);
        cmd.args(["--effort", &self.effort]);
        cmd.args([
            "--system-prompt-file",
            &prompt_file.path().to_string_lossy(),
        ]);

        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // This runs once per inference call, so on Windows it would be a
        // console window per turn of every agent.
        leviath_sys::hide_console_window(cmd.as_std_mut());

        let mut child = retry_etxtbsy(|| cmd.spawn()).await.map_err(|e| {
            // Permanent: a missing or unusable binary will not fix itself, and
            // `RequestFailed` would have the retry policy loop on it forever.
            ProviderError::Other(format!(
                "Failed to spawn '{}': {}. Is Claude Code installed?",
                self.binary_path, e
            ))
        })?;

        let mut stdin = child
            .stdin
            .take()
            .expect("stdin pipe was configured - take() always succeeds");
        let prompt = text_tools::flatten_messages(&request.messages);
        // Feed stdin from a detached task, for two reasons. First, it runs
        // concurrently with reading stdout, so a large response can't deadlock
        // against a large prompt (each side blocked waiting for the other to
        // drain). Second, the write result is deliberately ignored: a CLI that
        // exits before draining stdin closes the pipe, and that broken pipe is
        // not our failure - the child's exit status and stderr are what matter,
        // and letting the write error win would mask them (e.g. hide a nonzero
        // exit's diagnostics behind "broken pipe"). Dropping `stdin` when the
        // task ends closes it, signalling EOF.
        let writer = tokio::spawn(async move {
            let _ = stdin.write_all(prompt.as_bytes()).await;
        });
        let output = tokio::time::timeout(timeout_duration, child.wait_with_output())
            .await
            .map_err(|_| {
                ProviderError::RequestFailed(format!(
                    "Claude Code process timed out after {}s",
                    timeout_duration.as_secs()
                ))
            })?
            .expect("wait_with_output cannot fail for a normally-spawned process");
        // The writer has finished (the child exited, closing the pipe); join it
        // so no task is left dangling.
        let _ = writer.await;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ProviderError::RequestFailed(format!(
                "Claude Code exited with status {}: {}",
                output.status, stderr
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value = serde_json::from_str(&stdout).map_err(|e| {
            ProviderError::InvalidResponse(format!(
                "Failed to parse Claude Code JSON response: {e}"
            ))
        })?;
        self.parse_response(&json)
    }

    /// Turn the CLI's result JSON into an [`InferenceResponse`], splitting the
    /// reply text into prose and tool calls.
    fn parse_response(&self, json: &serde_json::Value) -> Result<InferenceResponse> {
        if json
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return Err(classify_error(json));
        }

        let raw = json.get("result").and_then(|v| v.as_str()).unwrap_or("");
        let (content, parsed_calls) = text_tools::parse_tool_calls(raw);
        let tool_calls = self.assign_ids(parsed_calls);

        let prompt_tokens = json
            .pointer("/usage/input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let completion_tokens = json
            .pointer("/usage/output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let cached_tokens = json
            .pointer("/usage/cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let cache_write_tokens = json
            .pointer("/usage/cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;

        // A reply that asked for tools finished for that reason, whatever the
        // CLI's own `stop_reason` says - it has no concept of our protocol.
        let finish_reason = if tool_calls.is_empty() {
            parse_stop_reason(json.get("stop_reason").and_then(|v| v.as_str()))
        } else {
            FinishReason::ToolCall
        };

        Ok(InferenceResponse {
            content,
            tool_calls,
            tokens_used: TokenUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
                cached_tokens,
                cache_write_tokens,
            },
            finish_reason,
        })
    }
}

/// Classify an `is_error` result so the retry policy does the right thing.
///
/// [`ProviderError::is_transient`] treats every `RequestFailed` as retryable, so
/// permanent conditions have to come back as something else or the engine will
/// loop on them forever.
fn classify_error(json: &serde_json::Value) -> ProviderError {
    let text = json
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown error from Claude Code");
    let lowered = text.to_ascii_lowercase();

    // Subscription caps are worth backing off for.
    let status_429 = json
        .get("api_error_status")
        .and_then(|v| v.as_u64())
        .is_some_and(|s| s == 429);
    if status_429
        || lowered.contains("rate limit")
        || lowered.contains("429")
        || lowered.contains("too many requests")
        || lowered.contains("usage limit")
    {
        return ProviderError::RateLimitExceeded;
    }

    // Not authenticated: permanent until the user acts. `ApiError` carries none
    // of the substrings `is_transient` looks for, so it stays permanent.
    if lowered.contains("not logged in") || lowered.contains("/login") {
        return ProviderError::ApiError(format!(
            "{text} - the Claude Code CLI is not authenticated. Run `claude` and sign in, \
             or use a direct provider with an API key."
        ));
    }

    ProviderError::ApiError(text.to_string())
}

/// Staging the system prompt failed. Permanent: an unwritable temp directory or
/// a full disk won't clear itself, and `RequestFailed` would have the retry
/// policy loop on it.
fn prompt_file_error(e: std::io::Error) -> ProviderError {
    ProviderError::Other(format!(
        "Failed to stage the Claude Code system prompt file: {e}"
    ))
}

/// Write the system prompt to a fresh temp file in `dir` and return the handle.
///
/// The file is created 0600 by `tempfile` - the prompt carries the user's task
/// and their code, so it must not be world-readable even briefly - and is
/// removed when the returned guard drops, including on every error path in the
/// caller.
///
/// `dir` is a parameter rather than an implicit `std::env::temp_dir()` so the
/// creation-failure path is reachable from a test by passing a directory that
/// doesn't exist. Mutating `TMPDIR` instead would race every other test that
/// creates a temp file. Only *creation* is fallible; writing to the file we just
/// created and privately own is treated as infallible (the same stance the
/// subprocess wait below takes), so it adds no unreachable error arm.
fn stage_prompt_file(
    dir: &std::path::Path,
    contents: &str,
) -> std::io::Result<tempfile::NamedTempFile> {
    let mut file = tempfile::Builder::new()
        .prefix("lev-claude-prompt-")
        .tempfile_in(dir)?;
    file.write_all(contents.as_bytes())
        .and_then(|()| file.flush())
        .expect("writing to a freshly created private temp file cannot fail");
    Ok(file)
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

/// Run `op` (a spawn attempt), retrying briefly when it fails with
/// `ETXTBSY`. On POSIX, `exec` fails with "Text file busy" while *any*
/// process holds a write handle to the executable - including a write fd
/// inherited by another process's in-flight fork/exec, or an installer
/// rewriting the claude binary mid-spawn. The condition clears as soon as
/// the writer closes, so a short bounded retry is the standard remedy
/// (cargo does the same). On Windows the error kind never occurs, so this
/// is a plain pass-through there.
async fn retry_etxtbsy<T>(mut op: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    const MAX_RETRIES: u32 = 40;
    let mut attempt = 0;
    loop {
        match op() {
            Err(e)
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < MAX_RETRIES =>
            {
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            other => return other,
        }
    }
}

#[async_trait]
impl Provider for ClaudeCodeProvider {
    async fn infer(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        // Honor a per-stage `request_timeout_secs`; fall back to the shared
        // default so this provider is bounded the same way as the HTTP ones.
        let timeout = std::time::Duration::from_secs(
            request
                .request_timeout_secs
                .unwrap_or(crate::provider::DEFAULT_INFERENCE_TIMEOUT_SECS),
        );
        self.infer_with_timeout(request, &std::env::temp_dir(), timeout)
            .await
    }

    async fn count_tokens(&self, text: &str, _model: &str) -> usize {
        // Subprocess transport with no token-count endpoint; ~3.5 chars/token
        // heuristic (same as the Anthropic provider's fallback).
        (text.len() as f64 / 3.5).ceil() as usize
    }

    fn max_context_tokens(&self, model: &str) -> usize {
        if let Some(caps) = self.capability_overrides.get(model) {
            return caps.max_context_tokens;
        }
        200_000 - INJECTION_RESERVE_TOKENS
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
        let models = [
            ("claude-opus-5", "Claude Opus 5"),
            ("claude-sonnet-5", "Claude Sonnet 5"),
            ("claude-opus-4-8", "Claude Opus 4.8"),
            ("claude-sonnet-4-6", "Claude Sonnet 4.6"),
            ("claude-haiku-4-5", "Claude Haiku 4.5"),
        ];
        Ok(models
            .into_iter()
            .map(|(id, display)| ModelInfo {
                id: id.to_string(),
                display_name: Some(display.to_string()),
                provider: "claude-code".to_string(),
                capabilities: self.builtin_capabilities(id),
            })
            .collect())
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

    // ─── construction / configuration ───────────────────────────────────────

    #[test]
    fn new_uses_default_binary_and_effort() {
        let provider = ClaudeCodeProvider::new();
        assert_eq!(provider.binary_path, "claude");
        assert_eq!(provider.effort, DEFAULT_EFFORT);
    }

    #[test]
    fn with_binary_path_sets_custom_path() {
        let provider = ClaudeCodeProvider::with_binary_path("/usr/local/bin/claude".to_string());
        assert_eq!(provider.binary_path, "/usr/local/bin/claude");
        assert_eq!(provider.effort, DEFAULT_EFFORT);
    }

    #[test]
    fn every_documented_effort_level_is_accepted() {
        for level in EFFORT_LEVELS {
            let provider = ClaudeCodeProvider::with_overrides(
                "claude".to_string(),
                Some(level.to_string()),
                None,
            );
            assert_eq!(provider.effort, level);
        }
    }

    #[test]
    fn unknown_effort_falls_back_to_the_default() {
        // Passing it through would make the CLI reject every call.
        let provider = ClaudeCodeProvider::with_overrides(
            "claude".to_string(),
            Some("turbo".to_string()),
            None,
        );
        assert_eq!(provider.effort, DEFAULT_EFFORT);
    }

    #[test]
    fn with_overrides_applies_capabilities() {
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
        let provider =
            ClaudeCodeProvider::with_overrides("claude".to_string(), None, Some(overrides));
        let caps = provider.capabilities("custom-model");
        assert!(caps.supports_temperature);
        assert!(!caps.supports_streaming);
        assert_eq!(caps.max_context_tokens, 100_000);
        assert_eq!(provider.max_context_tokens("custom-model"), 100_000);
    }

    #[test]
    fn with_overrides_none_leaves_capabilities_empty() {
        let provider = ClaudeCodeProvider::with_overrides("claude".to_string(), None, None);
        assert!(provider.capability_overrides.is_empty());
    }

    #[test]
    fn default_impl_matches_new() {
        let provider = ClaudeCodeProvider::default();
        assert_eq!(provider.binary_path, "claude");
        assert_eq!(provider.name(), "claude-code");
    }

    // ─── capabilities ───────────────────────────────────────────────────────

    #[test]
    fn capabilities_report_synthesized_tools_and_no_streaming() {
        let provider = ClaudeCodeProvider::new();
        let caps = provider.capabilities("claude-sonnet-4-6");
        assert!(!caps.supports_temperature);
        assert!(!caps.supports_streaming);
        assert!(caps.supports_tools);
        assert!(caps.supports_system_prompt);
    }

    #[test]
    fn context_window_reserves_room_for_cli_injections() {
        let provider = ClaudeCodeProvider::new();
        assert_eq!(provider.max_context_tokens("claude-sonnet-4-6"), 198_000);
        assert_eq!(provider.max_context_tokens("anything-unknown"), 198_000);
        assert_eq!(
            provider
                .capabilities("claude-sonnet-4-6")
                .max_context_tokens,
            198_000
        );
    }

    #[test]
    fn output_limits_vary_by_model_family() {
        let provider = ClaudeCodeProvider::new();
        assert_eq!(
            provider.capabilities("claude-opus-4-8").max_output_tokens,
            32_000
        );
        assert_eq!(
            provider.capabilities("claude-haiku-4-5").max_output_tokens,
            8_192
        );
        assert_eq!(
            provider.capabilities("claude-sonnet-4-6").max_output_tokens,
            16_000
        );
    }

    #[tokio::test]
    async fn count_tokens_uses_the_character_heuristic() {
        let provider = ClaudeCodeProvider::new();
        assert_eq!(provider.count_tokens("", "claude-sonnet-4-6").await, 0);
        assert_eq!(
            provider
                .count_tokens(&"a".repeat(350), "claude-sonnet-4-6")
                .await,
            100
        );
    }

    #[test]
    fn name_is_the_registry_key() {
        assert_eq!(ClaudeCodeProvider::new().name(), "claude-code");
    }

    #[tokio::test]
    async fn list_models_covers_the_current_families() {
        let provider = ClaudeCodeProvider::new();
        let models = provider.list_models().await.unwrap();
        assert_eq!(models.len(), 5);
        assert!(models.iter().any(|m| m.id == "claude-opus-5"));
        assert!(models.iter().any(|m| m.id == "claude-sonnet-5"));
        assert!(models.iter().any(|m| m.id == "claude-sonnet-4-6"));
        assert!(models.iter().any(|m| m.id == "claude-opus-4-8"));
        assert!(models.iter().any(|m| m.id == "claude-haiku-4-5"));
        for model in &models {
            assert_eq!(model.provider, "claude-code");
            assert!(model.display_name.is_some());
        }
    }

    // ─── build_system_prompt ────────────────────────────────────────────────

    #[test]
    fn system_blocks_reach_the_prompt() {
        // `assemble()` puts every structured region in `request.system`;
        // discarding those blocks silently drops every region from the prompt.
        let mut req = make_request();
        req.system = vec![
            SystemBlock {
                text: "[architecture]:\nhexagonal".to_string(),
                cache_hint: leviath_core::CacheHint::Always,
            },
            SystemBlock {
                text: "[plan]:\nstep one".to_string(),
                cache_hint: leviath_core::CacheHint::Never,
            },
        ];
        let prompt = ClaudeCodeProvider::build_system_prompt(&req);
        assert!(prompt.contains("[architecture]:\nhexagonal"));
        assert!(prompt.contains("[plan]:\nstep one"));
    }

    #[test]
    fn empty_system_blocks_are_skipped() {
        let mut req = make_request();
        req.system = vec![
            SystemBlock {
                text: String::new(),
                cache_hint: leviath_core::CacheHint::Never,
            },
            SystemBlock {
                text: "kept".to_string(),
                cache_hint: leviath_core::CacheHint::Never,
            },
        ];
        assert_eq!(ClaudeCodeProvider::build_system_prompt(&req), "kept");
    }

    #[test]
    fn tool_catalog_is_appended_only_when_the_stage_has_tools() {
        let req = make_request();
        assert!(!ClaudeCodeProvider::build_system_prompt(&req).contains(text_tools::FENCE_TAG));

        let mut with_tools = make_request();
        with_tools.tools = vec![Tool {
            name: "read_file".to_string(),
            description: "read a file".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let prompt = ClaudeCodeProvider::build_system_prompt(&with_tools);
        assert!(prompt.contains("- read_file: read a file"));
        assert!(prompt.contains(text_tools::FENCE_TAG));
    }

    // ─── id assignment ──────────────────────────────────────────────────────

    #[test]
    fn tool_call_ids_are_unique_across_responses() {
        let provider = ClaudeCodeProvider::new();
        let first = provider.assign_ids(vec![
            ("a".to_string(), serde_json::json!({})),
            ("b".to_string(), serde_json::json!({})),
        ]);
        let second = provider.assign_ids(vec![("c".to_string(), serde_json::json!({}))]);
        assert_eq!(first[0].id, "cc_call_1");
        assert_eq!(first[1].id, "cc_call_2");
        // Not restarted at 1 - a transcript pairs results to ids by name.
        assert_eq!(second[0].id, "cc_call_3");
        assert_eq!(second[0].name, "c");
    }

    // ─── parse_response ─────────────────────────────────────────────────────

    fn parse(json: &str) -> Result<InferenceResponse> {
        ClaudeCodeProvider::new().parse_response(&serde_json::from_str(json).unwrap())
    }

    #[test]
    fn parses_a_plain_text_reply() {
        let response = parse(
            r#"{"is_error": false, "result": "Hello!", "stop_reason": "end_turn",
                "usage": {"input_tokens": 42, "output_tokens": 15}}"#,
        )
        .unwrap();
        assert_eq!(response.content, "Hello!");
        assert!(response.tool_calls.is_empty());
        assert_eq!(response.tokens_used.prompt_tokens, 42);
        assert_eq!(response.tokens_used.completion_tokens, 15);
        assert_eq!(response.tokens_used.total_tokens, 57);
        assert_eq!(response.finish_reason, FinishReason::Complete);
    }

    #[test]
    fn parses_cache_usage_when_present() {
        let response = parse(
            r#"{"is_error": false, "result": "hi",
                "usage": {"input_tokens": 1, "output_tokens": 1,
                          "cache_read_input_tokens": 7, "cache_creation_input_tokens": 9}}"#,
        )
        .unwrap();
        assert_eq!(response.tokens_used.cached_tokens, 7);
        assert_eq!(response.tokens_used.cache_write_tokens, 9);
    }

    #[test]
    fn extracts_tool_calls_from_the_reply() {
        let response = parse(
            r#"{"is_error": false, "stop_reason": "end_turn",
                "result": "Reading it.\n\n```leviath-tool-calls\n[{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.rs\"}}]\n```",
                "usage": {"input_tokens": 1, "output_tokens": 1}}"#,
        )
        .unwrap();
        assert_eq!(response.content, "Reading it.");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "read_file");
        assert_eq!(response.tool_calls[0].arguments["path"], "a.rs");
        // Overrides the CLI's `end_turn`, which knows nothing of our protocol.
        assert_eq!(response.finish_reason, FinishReason::ToolCall);
    }

    #[test]
    fn missing_fields_degrade_to_empty_values() {
        let response = parse(r#"{"type": "result"}"#).unwrap();
        assert_eq!(response.content, "");
        assert_eq!(response.tokens_used.total_tokens, 0);
        assert_eq!(response.finish_reason, FinishReason::Complete);
    }

    #[test]
    fn stop_reason_variants_map_through() {
        assert_eq!(parse_stop_reason(Some("end_turn")), FinishReason::Complete);
        assert_eq!(parse_stop_reason(Some("stop")), FinishReason::Complete);
        assert_eq!(parse_stop_reason(Some("tool_use")), FinishReason::ToolCall);
        assert_eq!(
            parse_stop_reason(Some("max_tokens")),
            FinishReason::TokenLimit
        );
        assert_eq!(parse_stop_reason(None), FinishReason::Complete);
        assert_eq!(parse_stop_reason(Some("mystery")), FinishReason::Complete);
    }

    #[test]
    fn token_limit_survives_when_no_tools_were_called() {
        let response = parse(
            r#"{"is_error": false, "result": "truncated", "stop_reason": "max_tokens",
                "usage": {"input_tokens": 1, "output_tokens": 1}}"#,
        )
        .unwrap();
        assert_eq!(response.finish_reason, FinishReason::TokenLimit);
    }

    // ─── error classification ───────────────────────────────────────────────

    fn classify(json: &str) -> ProviderError {
        parse(json).unwrap_err()
    }

    #[test]
    fn logged_out_is_permanent_and_actionable() {
        let err = classify(r#"{"is_error": true, "result": "Not logged in · Please run /login"}"#);
        let msg = err.to_string();
        assert!(msg.contains("not authenticated"), "{msg}");
        assert!(
            !err.is_transient(),
            "a logged-out CLI must not be retried forever"
        );
    }

    #[test]
    fn rate_limits_are_transient() {
        for body in [
            r#"{"is_error": true, "result": "Rate limit exceeded"}"#,
            r#"{"is_error": true, "result": "You have hit your usage limit"}"#,
            r#"{"is_error": true, "result": "HTTP 429 Too Many Requests"}"#,
            r#"{"is_error": true, "result": "nope", "api_error_status": 429}"#,
        ] {
            let err = classify(body);
            assert_eq!(
                std::mem::discriminant(&err),
                std::mem::discriminant(&ProviderError::RateLimitExceeded),
                "{body}"
            );
            assert!(err.is_transient(), "{body}");
        }
    }

    #[test]
    fn a_non_429_status_is_not_treated_as_a_rate_limit() {
        let err =
            classify(r#"{"is_error": true, "result": "bad request", "api_error_status": 400}"#);
        assert!(err.to_string().contains("bad request"));
        assert!(!err.is_transient());
    }

    #[test]
    fn other_errors_pass_their_text_through() {
        let err = classify(r#"{"is_error": true, "result": "model not found"}"#);
        assert!(err.to_string().contains("model not found"));
    }

    #[test]
    fn an_error_without_result_text_still_reports_something() {
        let err = classify(r#"{"is_error": true}"#);
        assert!(err.to_string().contains("Unknown error from Claude Code"));
    }

    // ─── retry_etxtbsy ──────────────────────────────────────────────────────
    //
    // Unit-tested with injected errors rather than real busy executables: macOS
    // does not enforce ETXTBSY at all, so a real-file simulation only exercises
    // the branches on Linux. `start_paused` fast-forwards the backoff sleeps.

    #[tokio::test(start_paused = true)]
    async fn retry_etxtbsy_retries_then_succeeds() {
        let mut calls = 0;
        let result = retry_etxtbsy(|| {
            calls += 1;
            if calls < 3 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::ExecutableFileBusy,
                    "Text file busy",
                ))
            } else {
                Ok(42)
            }
        })
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_etxtbsy_gives_up_when_busy_never_clears() {
        let mut calls = 0;
        let result: std::io::Result<()> = retry_etxtbsy(|| {
            calls += 1;
            Err(std::io::Error::new(
                std::io::ErrorKind::ExecutableFileBusy,
                "Text file busy",
            ))
        })
        .await;
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::ExecutableFileBusy
        );
        // Initial attempt plus MAX_RETRIES retries.
        assert_eq!(calls, 41);
    }

    #[tokio::test]
    async fn retry_etxtbsy_other_errors_pass_through_immediately() {
        let mut calls = 0;
        let result: std::io::Result<()> = retry_etxtbsy(|| {
            calls += 1;
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such file",
            ))
        })
        .await;
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::NotFound);
        assert_eq!(calls, 1);
    }

    // ─── infer(): stub `claude` binary via with_binary_path ─────────────────
    //
    // ClaudeCodeProvider shells out to a real subprocess, so exercising infer()
    // means substituting a fake "claude" binary - a small script that prints
    // canned output - via the existing `with_binary_path` test seam.
    //
    // A `#!/bin/sh` shebang script (`chmod +x`'d and spawned directly) is
    // Unix-only: Windows' `CreateProcess` doesn't understand shebangs and can't
    // execute a `.sh` file as a native binary at all - every test using one
    // failed on Windows CI with "%1 is not a valid Win32 application" (os error
    // 193). `.bat` files, on the other hand, Windows *can* launch directly via
    // `Command::new(path)`.
    //
    // `write_stub_script` therefore takes a body for each syntax and internally
    // writes whichever one applies to the target platform - so each test below
    // is a single, platform-agnostic function, just parameterized on two small
    // strings expressing the same canned behavior in each shell's syntax.
    fn write_stub_script(tag: &str, sh_body: &str, bat_body: &str) -> std::path::PathBuf {
        #[cfg(unix)]
        {
            let _ = bat_body;
            let path = stub_path(tag, "sh");
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "#!/bin/sh").unwrap();
            f.write_all(sh_body.as_bytes()).unwrap();
            f.sync_all().unwrap();
            drop(f);
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
        #[cfg(windows)]
        {
            let _ = sh_body;
            let path = stub_path(tag, "bat");
            std::fs::write(&path, format!("@echo off\r\n{}\r\n", bat_body)).unwrap();
            path
        }
    }

    fn stub_path(tag: &str, ext: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "lev-claude-stub-{}-{}.{}",
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            ext
        ))
    }

    fn make_request() -> InferenceRequest {
        InferenceRequest {
            system: vec![],
            messages: vec![Message {
                role: "user".to_string(),
                content: "hi".into(),
                cache_breakpoint: false,
            }],
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 100,
            temperature: 0.0,
            tools: vec![],
            extra: serde_json::Value::Null,
            request_timeout_secs: None,
        }
    }

    #[tokio::test]
    async fn infer_success_parses_response() {
        let script = write_stub_script(
            "infer-ok",
            "echo '{\"result\": \"hello from stub\", \"usage\": {\"input_tokens\": 3, \"output_tokens\": 2}}'\n",
            "echo {\"result\": \"hello from stub\", \"usage\": {\"input_tokens\": 3, \"output_tokens\": 2}}",
        );
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        let resp = provider.infer(&make_request()).await.unwrap();
        assert_eq!(resp.content, "hello from stub");
        let _ = std::fs::remove_file(&script);
    }

    /// The argv is the contract with the CLI, and two of its entries are
    /// load-bearing absences: `--bare` breaks OAuth (every subscription call
    /// fails "Not logged in"), and `--allowed-tools` would hand Leviath's tool
    /// names to Claude Code's own executor. A stub that dumps its arguments is
    /// the only way to hold that line.
    #[tokio::test]
    async fn infer_builds_a_locked_down_argv() {
        let script = write_stub_script(
            "infer-argv",
            "echo \"$@\" >&2\necho '{\"result\": \"ok\"}'\n",
            "echo %* 1>&2\r\necho {\"result\": \"ok\"}",
        );
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        // Re-run the command capture directly so we can read stderr.
        //
        // Through `retry_etxtbsy` for the same reason production spawns are: a
        // write fd inherited by another test's in-flight fork makes `exec` fail
        // with "Text file busy" even though this stub was written, synced and
        // closed before `chmod`. It is a race against the rest of the suite, so
        // it fails perhaps one CI run in twenty and always somewhere unrelated.
        let out = retry_etxtbsy(|| {
            std::process::Command::new(&script)
                .args([
                    "--print",
                    "--output-format",
                    "json",
                    "--no-session-persistence",
                    "--tools",
                    "",
                    "--setting-sources",
                    "",
                    "--strict-mcp-config",
                    "--disable-slash-commands",
                    "--model",
                    "claude-sonnet-4-6",
                    "--effort",
                    "medium",
                    "--system-prompt-file",
                    "/tmp/x",
                ])
                .output()
        })
        .await
        .expect("the stub runs once no other process holds a write fd to it");
        let argv = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(!argv.contains("--bare"), "argv must never carry --bare");
        assert!(!argv.contains("--allowed-tools"));
        assert!(argv.contains("--system-prompt-file"));
        assert!(argv.contains("--effort medium"));
        // And the provider itself still completes against the same stub.
        assert_eq!(provider.infer(&make_request()).await.unwrap().content, "ok");
        let _ = std::fs::remove_file(&script);
    }

    #[tokio::test]
    async fn infer_sends_the_transcript_on_stdin() {
        // `cat` the prompt back out inside a JSON result: proves the flattened
        // transcript actually reaches the child rather than being dropped.
        let script = write_stub_script(
            "infer-stdin",
            "P=$(cat)\necho \"{\\\"result\\\": \\\"saw:$P\\\"}\"\n",
            "set /p P=\r\necho {\"result\": \"saw:%P%\"}",
        );
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        let resp = provider.infer(&make_request()).await.unwrap();
        assert!(resp.content.contains("User: hi"), "got {:?}", resp.content);
        let _ = std::fs::remove_file(&script);
    }

    #[tokio::test]
    async fn infer_with_timeout_fires_on_slow_process() {
        // A stub that outlives a short injected timeout, exercising the real
        // `tokio::time::timeout` branch - `infer()` hardcodes a 5-minute
        // timeout, far too long to wait for in a test. Windows has no `sleep`;
        // `ping -n 6 127.0.0.1` is the standard batch-file substitute.
        let script = write_stub_script(
            "infer-slow",
            "sleep 5\necho '{\"result\": \"late\"}'\n",
            "ping -n 6 127.0.0.1 >nul\r\necho {\"result\": \"late\"}",
        );
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        let err = provider
            .infer_with_timeout(
                &make_request(),
                &std::env::temp_dir(),
                std::time::Duration::from_millis(100),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        let _ = std::fs::remove_file(&script);
    }

    #[tokio::test]
    async fn infer_honors_per_request_timeout() {
        // `infer()` must read a per-stage `request_timeout_secs` and abort the
        // subprocess at that deadline instead of the 15-minute default - proving
        // the per-stage timeout reaches this (non-HTTP) provider too.
        let script = write_stub_script(
            "infer-per-req",
            "sleep 5\necho '{\"result\": \"late\"}'\n",
            "ping -n 6 127.0.0.1 >nul\r\necho {\"result\": \"late\"}",
        );
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        let mut request = make_request();
        request.request_timeout_secs = Some(1);
        let err = provider.infer(&request).await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        let _ = std::fs::remove_file(&script);
    }

    #[tokio::test]
    async fn infer_reports_a_prompt_staging_failure() {
        // A temp dir that doesn't exist makes prompt staging fail inside infer,
        // exercising the `map_err(prompt_file_error)?` arm and confirming the
        // failure is permanent (an unwritable temp dir won't fix itself).
        let provider = ClaudeCodeProvider::new();
        let missing = std::env::temp_dir().join("lev-cc-infer-no-dir-7z8y9");
        let err = provider
            .infer_with_timeout(&make_request(), &missing, std::time::Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("stage the Claude Code system prompt"),
            "{err}"
        );
        assert!(!err.is_transient());
    }

    /// A CLI that exits nonzero without draining stdin must still report its
    /// real exit status and stderr - the broken pipe from the undrained write is
    /// swallowed, not surfaced in its place. Uses a payload larger than the pipe
    /// buffer so the write genuinely races the early exit (the exact shape where
    /// a "broken pipe" error can mask the nonzero-exit diagnostics).
    #[cfg(unix)]
    #[tokio::test]
    async fn infer_reports_exit_status_even_when_stdin_is_not_drained() {
        let script = write_stub_script(
            "infer-earlyexit",
            "echo 'boom' >&2\nexit 3\n",
            "echo boom 1>&2\r\nexit /b 3",
        );
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        let mut req = make_request();
        // >1 MiB so the write cannot be swallowed whole by the pipe buffer.
        req.messages[0].content = "x".repeat(2 * 1024 * 1024).into();
        let err = provider.infer(&req).await.unwrap_err();
        assert!(
            err.to_string().contains("boom"),
            "the child's stderr must survive an undrained stdin; got: {err}"
        );
        let _ = std::fs::remove_file(&script);
    }

    #[tokio::test]
    async fn infer_error_result_is_classified() {
        let script = write_stub_script(
            "infer-err",
            "echo '{\"is_error\": true, \"result\": \"bad request\"}'\n",
            "echo {\"is_error\": true, \"result\": \"bad request\"}",
        );
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        let err = provider.infer(&make_request()).await.unwrap_err();
        assert!(err.to_string().contains("bad request"));
        let _ = std::fs::remove_file(&script);
    }

    #[tokio::test]
    async fn infer_nonzero_exit_returns_request_failed() {
        let script = write_stub_script(
            "infer-fail",
            "echo 'boom' >&2\nexit 1\n",
            "echo boom 1>&2\r\nexit /b 1",
        );
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        let err = provider.infer(&make_request()).await.unwrap_err();
        assert!(err.to_string().contains("boom"));
        let _ = std::fs::remove_file(&script);
    }

    #[tokio::test]
    async fn infer_unparseable_stdout_is_an_invalid_response() {
        let script = write_stub_script(
            "infer-garbage",
            "echo 'not json at all'\n",
            "echo not json at all",
        );
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        let err = provider.infer(&make_request()).await.unwrap_err();
        assert!(err.to_string().starts_with("Invalid response:"), "{err}");
        assert!(!err.is_transient());
        let _ = std::fs::remove_file(&script);
    }

    // ─── staging the system prompt file ─────────────────────────────────────

    #[test]
    fn staging_writes_the_prompt_to_a_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = stage_prompt_file(dir.path(), "system prompt body").unwrap();
        assert_eq!(
            std::fs::read_to_string(file.path()).unwrap(),
            "system prompt body"
        );
        // The prompt carries the user's task and their code; it must not be
        // readable by other users even for the life of one call.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(file.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "mode was {:o}", mode);
        }
    }

    #[test]
    fn staging_into_a_missing_directory_is_a_permanent_error() {
        let missing = std::env::temp_dir().join("lev-cc-no-such-dir-1a2b3c");
        let err = stage_prompt_file(&missing, "body").unwrap_err();
        let mapped = prompt_file_error(err);
        assert!(
            mapped
                .to_string()
                .contains("stage the Claude Code system prompt"),
            "{mapped}"
        );
        assert!(
            !mapped.is_transient(),
            "an unwritable temp directory must not be retried forever"
        );
    }

    #[test]
    fn the_staged_file_is_removed_when_the_guard_drops() {
        let dir = tempfile::tempdir().unwrap();
        let path = stage_prompt_file(dir.path(), "body")
            .unwrap()
            .path()
            .to_path_buf();
        assert!(!path.exists(), "the prompt file must not outlive the call");
    }

    #[tokio::test]
    async fn infer_missing_binary_is_permanent() {
        let provider = ClaudeCodeProvider::with_binary_path(
            "/nonexistent/definitely/not/a/real/binary".to_string(),
        );
        let err = provider.infer(&make_request()).await.unwrap_err();
        assert!(err.to_string().contains("Is Claude Code installed?"));
        assert!(
            !err.is_transient(),
            "a missing binary must not be retried forever"
        );
    }

    #[tokio::test]
    async fn infer_round_trips_tools_end_to_end() {
        // The whole point of the rewrite: a stage with tools gets a catalog in
        // its system prompt and gets structured tool calls back out.
        // `printf '%s\n'` rather than `echo`: dash's echo expands the `\n`
        // escapes inside the JSON string into real newlines, which is invalid
        // JSON. printf copies a `%s` argument through untouched.
        let script = write_stub_script(
            "infer-tools",
            "printf '%s\\n' '{\"result\": \"On it.\\n```leviath-tool-calls\\n[{\\\"name\\\":\\\"read_file\\\",\\\"arguments\\\":{\\\"path\\\":\\\"a.rs\\\"}}]\\n```\"}'\n",
            "echo {\"result\": \"On it.\\n```leviath-tool-calls\\n[{\\\"name\\\":\\\"read_file\\\",\\\"arguments\\\":{\\\"path\\\":\\\"a.rs\\\"}}]\\n```\"}",
        );
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        let mut req = make_request();
        req.tools = vec![Tool {
            name: "read_file".to_string(),
            description: "read a file".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let resp = provider.infer(&req).await.unwrap();
        assert_eq!(resp.content, "On it.");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "read_file");
        assert_eq!(resp.finish_reason, FinishReason::ToolCall);
        let _ = std::fs::remove_file(&script);
    }

    /// Holds the stub script open for writing so exec fails `ETXTBSY`, then
    /// releases it mid-retry: the spawn must ride out the busy window and the
    /// inference must still succeed. Deterministic re-creation of the race that
    /// made stub tests flake on Linux CI.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_retries_until_etxtbsy_writer_releases() {
        let script = write_stub_script(
            "etxtbsy-retry",
            "echo '{\"result\": \"hello from stub\"}'\n",
            "",
        );
        let holder = std::fs::OpenOptions::new()
            .append(true)
            .open(&script)
            .unwrap();
        let release = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            drop(holder);
        });
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        let resp = provider.infer(&make_request()).await.unwrap();
        assert_eq!(resp.content, "hello from stub");
        release.await.unwrap();
        let _ = std::fs::remove_file(&script);
    }

    /// `infer_stream` is not overridden; the trait default wraps `infer`. The
    /// previous hand-written NDJSON stream was both unused by the runtime and
    /// wrong (it matched a top-level `content` string that the CLI never emits),
    /// so it was deleted rather than fixed.
    #[tokio::test]
    async fn infer_stream_falls_back_to_the_trait_default() {
        use tokio_stream::StreamExt;
        let script = write_stub_script(
            "stream-default",
            "echo '{\"result\": \"streamed\"}'\n",
            "echo {\"result\": \"streamed\"}",
        );
        let provider = ClaudeCodeProvider::with_binary_path(script.to_str().unwrap().to_string());
        let mut stream = provider.infer_stream(&make_request()).await.unwrap();
        let chunk = stream.next().await.unwrap().unwrap();
        assert_eq!(chunk.delta, "streamed");
        assert!(stream.next().await.is_none());
        let _ = std::fs::remove_file(&script);
    }
}
