//! File-based IPC channel for worker ↔ dashboard interaction.
//!
//! Background workers are headless (no stdin), so user interaction — free-text
//! questions, multiple-choice prompts, and tool-approval requests — flows
//! through two files under the run directory:
//!
//! - `pending.json`  — written by the worker, describes what it needs
//! - `response.json` — written by the dashboard (or `lev respond`),
//!   carries the user's answer
//!
//! Protocol:
//! 1. Worker writes `pending.json`, updates `meta.json` status → WaitingInput.
//! 2. Worker polls for `response.json` (100 ms intervals).
//! 3. Dashboard reads `pending.json` and renders the appropriate UI widget.
//! 4. User answers; dashboard writes `response.json`.
//! 5. Worker reads the response, deletes both files, resumes.
//!
//! The foreground (stdin) path uses the same `InteractionRequest`/`Response`
//! types but resolves them by reading stdin directly instead of polling files.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::runstate::{run_dir, write_meta, RunMeta, RunStatus};

// ─── Request ────────────────────────────────────────────────────────────────

/// The kind of interaction being requested.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum InteractionKind {
    /// Free-form text answer (the default today).
    FreeText,
    /// User picks one option from a numbered list.
    MultipleChoice,
    /// Yes/no confirmation.
    Confirm,
    /// Approve or deny a specific tool call before it executes.
    ToolApproval,
}

/// Format of an optional rich body attached to an interaction request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum BodyFormat {
    /// Plain text body (no special rendering).
    #[default]
    Plain,
    /// Markdown body — rendered via the dashboard's markdown renderer.
    Markdown,
}

/// A pending interaction request written by the worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionRequest {
    /// Unique ID for this request (uuid-lite: timestamp + stage index).
    pub id: String,
    /// What kind of answer is expected.
    pub kind: InteractionKind,
    /// Prompt text to display to the user.
    pub prompt: String,
    /// Options for MultipleChoice (index-labelled).
    #[serde(default)]
    pub options: Vec<String>,
    /// For ToolApproval: the tool name.
    pub tool_name: Option<String>,
    /// For ToolApproval: the tool arguments (JSON).
    pub tool_arguments: Option<serde_json::Value>,
    /// Whether an answer is mandatory (empty/cancel not allowed).
    #[serde(default = "default_true")]
    pub required: bool,
    /// Stage name that triggered this request.
    pub stage_name: String,
    /// Optional rich body (markdown document, plan, etc.) for the user to review.
    #[serde(default)]
    pub body: Option<String>,
    /// Format of the body content.
    #[serde(default)]
    pub body_format: BodyFormat,
}

fn default_true() -> bool {
    true
}

impl InteractionRequest {
    /// Create a new free-text request.
    pub fn free_text(
        id: impl Into<String>,
        prompt: impl Into<String>,
        stage: impl Into<String>,
        required: bool,
    ) -> Self {
        Self {
            id: id.into(),
            kind: InteractionKind::FreeText,
            prompt: prompt.into(),
            options: vec![],
            tool_name: None,
            tool_arguments: None,
            required,
            stage_name: stage.into(),
            body: None,
            body_format: BodyFormat::Plain,
        }
    }

    /// Create a "present for review" request: pauses the run and shows a rich
    /// markdown document to the user before accepting feedback.
    pub fn review(
        id: impl Into<String>,
        title: impl Into<String>,
        markdown: impl Into<String>,
        stage: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: InteractionKind::FreeText,
            prompt: title.into(),
            options: vec![],
            tool_name: None,
            tool_arguments: None,
            required: true,
            stage_name: stage.into(),
            body: Some(markdown.into()),
            body_format: BodyFormat::Markdown,
        }
    }

    /// Create a new multiple-choice request.
    pub fn multiple_choice(
        id: impl Into<String>,
        prompt: impl Into<String>,
        options: Vec<String>,
        stage: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: InteractionKind::MultipleChoice,
            prompt: prompt.into(),
            options,
            tool_name: None,
            tool_arguments: None,
            required: true,
            stage_name: stage.into(),
            body: None,
            body_format: BodyFormat::Plain,
        }
    }

    /// Create a new confirm request.
    pub fn confirm(
        id: impl Into<String>,
        prompt: impl Into<String>,
        stage: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: InteractionKind::Confirm,
            prompt: prompt.into(),
            options: vec!["Yes".to_string(), "No".to_string()],
            tool_name: None,
            tool_arguments: None,
            required: true,
            stage_name: stage.into(),
            body: None,
            body_format: BodyFormat::Plain,
        }
    }

    /// Create a new tool-approval request.
    pub fn tool_approval(
        id: impl Into<String>,
        tool_name: impl Into<String>,
        arguments: serde_json::Value,
        stage: impl Into<String>,
    ) -> Self {
        let tool = tool_name.into();
        let prompt = format!("Allow tool call: `{}`?", tool);
        Self {
            id: id.into(),
            kind: InteractionKind::ToolApproval,
            prompt,
            options: vec![
                "Allow once".to_string(),
                "Allow for this session".to_string(),
                "Deny".to_string(),
            ],
            tool_name: Some(tool),
            tool_arguments: Some(arguments),
            required: true,
            stage_name: stage.into(),
            body: None,
            body_format: BodyFormat::Plain,
        }
    }
}

// ─── Response ───────────────────────────────────────────────────────────────

/// The scope of an approval decision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalScope {
    /// Allow/deny just this one call.
    Once,
    /// Allow/deny all calls to this tool for the rest of this agent run.
    Session,
}

/// A response written by the dashboard (or `lev respond`) to answer the worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionResponse {
    /// Must match `InteractionRequest.id`.
    pub request_id: String,
    /// The user's free-text value (FreeText / InteractionPoints).
    pub value: Option<String>,
    /// Index into `options` for MultipleChoice / ToolApproval.
    pub choice_index: Option<usize>,
    /// Whether a ToolApproval was granted.
    pub approved: Option<bool>,
    /// Scope of a tool approval decision.
    pub scope: Option<ApprovalScope>,
}

impl InteractionResponse {
    /// Build a simple text response.
    pub fn text(request_id: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            request_id: request_id.into(),
            value: Some(value.into()),
            choice_index: None,
            approved: None,
            scope: None,
        }
    }

    /// Build a choice response (0-based index).
    pub fn choice(request_id: impl Into<String>, index: usize) -> Self {
        Self {
            request_id: request_id.into(),
            value: None,
            choice_index: Some(index),
            approved: None,
            scope: None,
        }
    }

    /// Build an approval response.
    pub fn approval(request_id: impl Into<String>, approved: bool, scope: ApprovalScope) -> Self {
        Self {
            request_id: request_id.into(),
            value: None,
            choice_index: None,
            approved: Some(approved),
            scope: Some(scope),
        }
    }
}

// ─── File paths ─────────────────────────────────────────────────────────────

pub fn pending_path(run_id: &str) -> PathBuf {
    run_dir(run_id).join("pending.json")
}

pub fn response_path(run_id: &str) -> PathBuf {
    run_dir(run_id).join("response.json")
}

// ─── Write helpers (used by the dashboard / `lev respond`) ─────────────────

/// Write an interaction request to disk (called by the worker).
pub fn write_request(run_id: &str, req: &InteractionRequest) -> anyhow::Result<()> {
    let path = pending_path(run_id);
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(req)?;
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Read the current interaction request for a run (used by the dashboard).
pub fn read_request(run_id: &str) -> Option<InteractionRequest> {
    let path = pending_path(run_id);
    let json = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&json).ok()
}

/// Write an interaction response to disk (called by the dashboard / `lev respond`).
pub fn write_response(run_id: &str, resp: &InteractionResponse) -> anyhow::Result<()> {
    let path = response_path(run_id);
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(resp)?;
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Attempt to read and atomically consume the response file (called by worker).
/// Returns `None` if no response has been written yet.
pub fn take_response(run_id: &str) -> Option<InteractionResponse> {
    let path = response_path(run_id);
    let json = std::fs::read_to_string(&path).ok()?;
    let resp: InteractionResponse = serde_json::from_str(&json).ok()?;
    // Remove the file so the worker only processes it once.
    let _ = std::fs::remove_file(&path);
    Some(resp)
}

/// Delete both pending and response files (cleanup after handling).
pub fn clear_interaction(run_id: &str) {
    let _ = std::fs::remove_file(pending_path(run_id));
    let _ = std::fs::remove_file(response_path(run_id));
}

// ─── Worker-side blocking request ───────────────────────────────────────────

/// Called from within a background worker to block until the user answers.
///
/// Synchronous variant — writes `pending.json`, flips `meta.status` to
/// `WaitingInput`, then polls `response.json` in 100 ms intervals. Only use
/// from non-async contexts; for async workers prefer `request_interaction_async`.
///
/// `timeout` controls the maximum wait; `None` means wait indefinitely.
#[allow(dead_code)]
pub fn request_interaction(
    run_id: &str,
    meta: &mut RunMeta,
    req: InteractionRequest,
    timeout: Option<Duration>,
) -> anyhow::Result<InteractionResponse> {
    // Write request, flip status
    write_request(run_id, &req)?;
    meta.status = if req.required {
        RunStatus::WaitingInput
    } else {
        RunStatus::CompleteInteractive
    };
    meta.touch();
    write_meta(meta)?;

    let started = Instant::now();

    loop {
        std::thread::sleep(Duration::from_millis(100));

        if let Some(resp) = take_response(run_id) {
            if !resp.request_id.is_empty() && resp.request_id != req.id {
                // Stale response for a different request — discard and keep waiting.
                continue;
            }
            // Clean up and resume
            clear_interaction(run_id);
            meta.status = RunStatus::Running;
            meta.touch();
            let _ = write_meta(meta);
            return Ok(resp);
        }

        if let Some(t) = timeout {
            if started.elapsed() >= t {
                clear_interaction(run_id);
                meta.status = RunStatus::Running;
                meta.touch();
                let _ = write_meta(meta);
                anyhow::bail!("Interaction timed out after {:.1}s", t.as_secs_f32());
            }
        }
    }
}

/// Async variant for use inside `tokio` contexts (background worker stages).
///
/// Same protocol as `request_interaction` but uses `tokio::time::sleep` so
/// it doesn't block the async runtime.
pub async fn request_interaction_async(
    run_id: &str,
    meta: &mut RunMeta,
    req: InteractionRequest,
    timeout: Option<Duration>,
) -> anyhow::Result<InteractionResponse> {
    write_request(run_id, &req)?;
    // Optional interactions (required: false) indicate post-completion follow-up;
    // show as CompleteInteractive so the dashboard doesn't offer a kill button.
    meta.status = if req.required {
        RunStatus::WaitingInput
    } else {
        RunStatus::CompleteInteractive
    };
    meta.touch();
    write_meta(meta)?;

    let started = Instant::now();

    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;

        if let Some(resp) = take_response(run_id) {
            if !resp.request_id.is_empty() && resp.request_id != req.id {
                // Stale response for a different request — discard and keep waiting.
                continue;
            }
            clear_interaction(run_id);
            meta.status = RunStatus::Running;
            meta.touch();
            let _ = write_meta(meta);
            return Ok(resp);
        }

        if let Some(t) = timeout {
            if started.elapsed() >= t {
                clear_interaction(run_id);
                meta.status = RunStatus::Running;
                meta.touch();
                let _ = write_meta(meta);
                anyhow::bail!("Interaction timed out after {:.1}s", t.as_secs_f32());
            }
        }
    }
}

/// Resolve a `FreeText` response to a string.
pub fn response_as_text(resp: &InteractionResponse) -> String {
    resp.value.clone().unwrap_or_default()
}

/// Resolve a `MultipleChoice` response to the chosen option string.
pub fn response_as_choice<'a>(
    resp: &InteractionResponse,
    options: &'a [String],
) -> Option<&'a String> {
    resp.choice_index.and_then(|i| options.get(i))
}

/// Returns `true` if a tool-approval response was granted.
pub fn response_approved(resp: &InteractionResponse) -> bool {
    resp.approved.unwrap_or(false)
}

/// Generate a simple monotonic ID from stage index + iteration.
pub fn make_interaction_id(stage_idx: usize, iteration: usize) -> String {
    format!("{}-{}", stage_idx, iteration)
}

/// Request tool approval from a background worker without a live RunMeta handle.
///
/// Reads `meta.json` from disk to update status, then polls for a response.
/// Returns `true` if approved (once or session), `false` if denied.
/// Also returns the `ApprovalScope` so callers can record session-level allows.
pub async fn request_tool_approval_background(
    run_id: &str,
    tool_name: &str,
    arguments: &serde_json::Value,
    stage_name: &str,
) -> (bool, ApprovalScope) {
    let req = InteractionRequest::tool_approval(
        make_interaction_id(
            // use a hash of tool name to get a stable-ish id within a stage
            tool_name
                .bytes()
                .fold(0usize, |a, b| a.wrapping_add(b as usize)),
            0,
        ),
        tool_name,
        arguments.clone(),
        stage_name,
    );

    // Update meta status to WaitingInput
    if let Ok(mut meta) = crate::runstate::read_meta(run_id) {
        let _ = write_request(run_id, &req);
        meta.status = RunStatus::WaitingInput;
        meta.touch();
        let _ = write_meta(&meta);
    } else {
        // If we can't read meta, just write the request and hope
        let _ = write_request(run_id, &req);
    }

    let started = Instant::now();
    let timeout = Duration::from_secs(300); // 5 minute timeout for tool approval

    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;

        if let Some(resp) = take_response(run_id) {
            if !resp.request_id.is_empty() && resp.request_id != req.id {
                // Stale response for a different request — discard and keep waiting.
                continue;
            }
            clear_interaction(run_id);
            // Restore Running status
            if let Ok(mut meta) = crate::runstate::read_meta(run_id) {
                meta.status = RunStatus::Running;
                meta.touch();
                let _ = write_meta(&meta);
            }
            let approved = resp.approved.unwrap_or(false);
            let scope = resp.scope.unwrap_or(ApprovalScope::Once);
            return (approved, scope);
        }

        if started.elapsed() >= timeout {
            clear_interaction(run_id);
            // Timeout → auto-deny (safe default)
            if let Ok(mut meta) = crate::runstate::read_meta(run_id) {
                meta.status = RunStatus::Running;
                meta.touch();
                let _ = write_meta(&meta);
            }
            return (false, ApprovalScope::Once);
        }
    }
}

/// Background helper for `present_for_review` — writes the request, sets
/// status to `WaitingInput`, then polls until the user responds.
///
/// Returns the `InteractionResponse`; never times out (review can take a while).
pub async fn request_interaction_bg_review(
    run_id: &str,
    req: InteractionRequest,
) -> InteractionResponse {
    // Write request and flip status
    let _ = write_request(run_id, &req);
    if let Ok(mut meta) = crate::runstate::read_meta(run_id) {
        meta.status = RunStatus::WaitingInput;
        meta.touch();
        let _ = write_meta(&meta);
    }

    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Some(resp) = take_response(run_id) {
            if !resp.request_id.is_empty() && resp.request_id != req.id {
                // Stale response — keep waiting
                continue;
            }
            clear_interaction(run_id);
            // Restore Running status
            if let Ok(mut meta) = crate::runstate::read_meta(run_id) {
                meta.status = RunStatus::Running;
                meta.touch();
                let _ = write_meta(&meta);
            }
            return resp;
        }
    }
}

// ─── Foreground (stdin) path ─────────────────────────────────────────────────

/// Request interaction from a foreground (stdin-connected) process.
///
/// Renders the prompt and reads stdin instead of using the file-based channel.
/// Returns the same `InteractionResponse` type for unified handling.
pub fn request_interaction_stdin(req: &InteractionRequest) -> InteractionResponse {
    use std::io::{self, Write};

    println!("\n[Interaction Point: {}]", req.stage_name);

    match req.kind {
        InteractionKind::FreeText => {
            println!("{}", req.prompt);
            if !req.required {
                println!("(Press Enter to skip)");
            }
            print!("You: ");
            io::stdout().flush().ok();
            let mut input = String::new();
            io::stdin().read_line(&mut input).ok();
            InteractionResponse::text(&req.id, input.trim())
        }

        InteractionKind::MultipleChoice => {
            println!("{}", req.prompt);
            for (i, opt) in req.options.iter().enumerate() {
                println!("  [{}] {}", i + 1, opt);
            }
            loop {
                print!("Choice (1-{}): ", req.options.len());
                io::stdout().flush().ok();
                let mut input = String::new();
                io::stdin().read_line(&mut input).ok();
                let s = input.trim();
                if let Ok(n) = s.parse::<usize>() {
                    if n >= 1 && n <= req.options.len() {
                        return InteractionResponse::choice(&req.id, n - 1);
                    }
                }
                println!("Please enter a number between 1 and {}.", req.options.len());
            }
        }

        InteractionKind::Confirm => loop {
            print!("{} [y/n]: ", req.prompt);
            io::stdout().flush().ok();
            let mut input = String::new();
            io::stdin().read_line(&mut input).ok();
            match input.trim().to_lowercase().as_str() {
                "y" | "yes" => {
                    return InteractionResponse::approval(&req.id, true, ApprovalScope::Once);
                }
                "n" | "no" => {
                    return InteractionResponse::approval(&req.id, false, ApprovalScope::Once);
                }
                _ => println!("Please enter y or n."),
            }
        },

        InteractionKind::ToolApproval => {
            if let Some(ref tool) = req.tool_name {
                println!("Tool call: `{}`", tool);
                if let Some(ref args) = req.tool_arguments {
                    println!(
                        "Arguments: {}",
                        serde_json::to_string_pretty(args).unwrap_or_default()
                    );
                }
            }
            println!("{}", req.prompt);
            for (i, opt) in req.options.iter().enumerate() {
                println!("  [{}] {}", i + 1, opt);
            }
            loop {
                print!("Choice (1-{}): ", req.options.len());
                io::stdout().flush().ok();
                let mut input = String::new();
                io::stdin().read_line(&mut input).ok();
                match input.trim() {
                    "1" => {
                        return InteractionResponse::approval(&req.id, true, ApprovalScope::Once);
                    }
                    "2" => {
                        return InteractionResponse::approval(
                            &req.id,
                            true,
                            ApprovalScope::Session,
                        );
                    }
                    "3" => {
                        return InteractionResponse::approval(&req.id, false, ApprovalScope::Once);
                    }
                    _ => println!("Please enter 1, 2, or 3."),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_builders() {
        let r = InteractionRequest::free_text("id1", "What now?", "plan", true);
        assert_eq!(r.kind, InteractionKind::FreeText);
        assert!(r.required);

        let r = InteractionRequest::multiple_choice(
            "id2",
            "Pick one",
            vec!["A".into(), "B".into()],
            "plan",
        );
        assert_eq!(r.kind, InteractionKind::MultipleChoice);
        assert_eq!(r.options.len(), 2);

        let r = InteractionRequest::tool_approval(
            "id3",
            "bash",
            serde_json::json!({"cmd": "ls"}),
            "impl",
        );
        assert_eq!(r.kind, InteractionKind::ToolApproval);
        assert_eq!(r.options.len(), 3);
    }

    #[test]
    fn test_response_builders() {
        let r = InteractionResponse::text("id1", "hello");
        assert_eq!(r.value.as_deref(), Some("hello"));

        let r = InteractionResponse::choice("id2", 1);
        assert_eq!(r.choice_index, Some(1));

        let r = InteractionResponse::approval("id3", true, ApprovalScope::Session);
        assert_eq!(r.approved, Some(true));
        assert_eq!(r.scope, Some(ApprovalScope::Session));
    }

    #[test]
    fn test_response_as_text() {
        let r = InteractionResponse::text("id", "answer");
        assert_eq!(response_as_text(&r), "answer");
        let empty = InteractionResponse {
            request_id: "x".into(),
            value: None,
            choice_index: None,
            approved: None,
            scope: None,
        };
        assert_eq!(response_as_text(&empty), "");
    }

    #[test]
    fn test_response_as_choice() {
        let opts = vec!["Alpha".to_string(), "Beta".to_string()];
        let r = InteractionResponse::choice("id", 0);
        assert_eq!(response_as_choice(&r, &opts), Some(&"Alpha".to_string()));
        let r = InteractionResponse::choice("id", 1);
        assert_eq!(response_as_choice(&r, &opts), Some(&"Beta".to_string()));
        let r = InteractionResponse::choice("id", 99);
        assert!(response_as_choice(&r, &opts).is_none());
    }

    #[test]
    fn test_make_interaction_id() {
        let id = make_interaction_id(2, 5);
        assert_eq!(id, "2-5");
    }

    // ─── InteractionRequest constructors ───────────────────────────────────

    #[test]
    fn test_free_text_request_not_required() {
        let r = InteractionRequest::free_text("ft1", "optional?", "stage1", false);
        assert_eq!(r.kind, InteractionKind::FreeText);
        assert!(!r.required);
        assert_eq!(r.id, "ft1");
        assert_eq!(r.prompt, "optional?");
        assert_eq!(r.stage_name, "stage1");
        assert!(r.options.is_empty());
        assert!(r.tool_name.is_none());
        assert!(r.tool_arguments.is_none());
        assert!(r.body.is_none());
        assert_eq!(r.body_format, BodyFormat::Plain);
    }

    #[test]
    fn test_review_request() {
        let r = InteractionRequest::review("rev1", "Review Title", "# Markdown body", "plan");
        assert_eq!(r.kind, InteractionKind::FreeText);
        assert!(r.required);
        assert_eq!(r.prompt, "Review Title");
        assert_eq!(r.body.as_deref(), Some("# Markdown body"));
        assert_eq!(r.body_format, BodyFormat::Markdown);
        assert_eq!(r.stage_name, "plan");
    }

    #[test]
    fn test_confirm_request() {
        let r = InteractionRequest::confirm("c1", "Proceed?", "deploy");
        assert_eq!(r.kind, InteractionKind::Confirm);
        assert_eq!(r.options, vec!["Yes", "No"]);
        assert!(r.required);
        assert_eq!(r.stage_name, "deploy");
    }

    #[test]
    fn test_tool_approval_request() {
        let args = serde_json::json!({"file": "test.txt"});
        let r = InteractionRequest::tool_approval("ta1", "write_file", args, "code");
        assert_eq!(r.kind, InteractionKind::ToolApproval);
        assert_eq!(r.tool_name.as_deref(), Some("write_file"));
        assert!(r.tool_arguments.is_some());
        assert_eq!(r.options.len(), 3);
        assert!(r.prompt.contains("write_file"));
    }

    // ─── InteractionResponse constructors ──────────────────────────────────

    #[test]
    fn test_response_text_empty() {
        let r = InteractionResponse::text("id", "");
        assert_eq!(r.value.as_deref(), Some(""));
        assert!(r.choice_index.is_none());
        assert!(r.approved.is_none());
        assert!(r.scope.is_none());
    }

    #[test]
    fn test_response_approval_denied() {
        let r = InteractionResponse::approval("id", false, ApprovalScope::Once);
        assert_eq!(r.approved, Some(false));
        assert_eq!(r.scope, Some(ApprovalScope::Once));
    }

    // ─── response_approved ─────────────────────────────────────────────────

    #[test]
    fn test_response_approved_true() {
        let r = InteractionResponse::approval("id", true, ApprovalScope::Session);
        assert!(response_approved(&r));
    }

    #[test]
    fn test_response_approved_false() {
        let r = InteractionResponse::approval("id", false, ApprovalScope::Once);
        assert!(!response_approved(&r));
    }

    #[test]
    fn test_response_approved_none() {
        let r = InteractionResponse::text("id", "hello");
        assert!(!response_approved(&r));
    }

    // ─── ApprovalScope serde ───────────────────────────────────────────────

    #[test]
    fn test_approval_scope_serde_roundtrip() {
        for scope in [ApprovalScope::Once, ApprovalScope::Session] {
            let json = serde_json::to_string(&scope).unwrap();
            let back: ApprovalScope = serde_json::from_str(&json).unwrap();
            assert_eq!(scope, back);
        }
    }

    #[test]
    fn test_approval_scope_snake_case() {
        let json = serde_json::to_string(&ApprovalScope::Once).unwrap();
        assert_eq!(json, "\"once\"");
        let json = serde_json::to_string(&ApprovalScope::Session).unwrap();
        assert_eq!(json, "\"session\"");
    }

    // ─── InteractionKind serde ─────────────────────────────────────────────

    #[test]
    fn test_interaction_kind_serde_roundtrip() {
        for kind in [
            InteractionKind::FreeText,
            InteractionKind::MultipleChoice,
            InteractionKind::Confirm,
            InteractionKind::ToolApproval,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: InteractionKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
    }

    // ─── BodyFormat serde ──────────────────────────────────────────────────

    #[test]
    fn test_body_format_serde_roundtrip() {
        for fmt in [BodyFormat::Plain, BodyFormat::Markdown] {
            let json = serde_json::to_string(&fmt).unwrap();
            let back: BodyFormat = serde_json::from_str(&json).unwrap();
            assert_eq!(fmt, back);
        }
    }

    #[test]
    fn test_body_format_default_is_plain() {
        let fmt = BodyFormat::default();
        assert_eq!(fmt, BodyFormat::Plain);
    }

    // ─── File I/O roundtrip (write_request, read_request, etc.) ────────────

    #[test]
    fn test_write_and_read_request() {
        let run_id = "test-interaction-rw-req";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let req = InteractionRequest::free_text("rw1", "What now?", "plan", true);
        write_request(run_id, &req).unwrap();

        let back = read_request(run_id);
        assert!(back.is_some());
        let back = back.unwrap();
        assert_eq!(back.id, "rw1");
        assert_eq!(back.prompt, "What now?");
        assert_eq!(back.kind, InteractionKind::FreeText);

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    #[test]
    fn test_write_and_read_response() {
        let run_id = "test-interaction-rw-resp";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let resp = InteractionResponse::text("rw2", "my answer");
        write_response(run_id, &resp).unwrap();

        let back = take_response(run_id);
        assert!(back.is_some());
        let back = back.unwrap();
        assert_eq!(back.request_id, "rw2");
        assert_eq!(back.value.as_deref(), Some("my answer"));

        // take_response should have removed the file
        assert!(take_response(run_id).is_none());

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    #[test]
    fn test_clear_interaction() {
        let run_id = "test-interaction-clear";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let req = InteractionRequest::free_text("c1", "prompt", "stage", true);
        write_request(run_id, &req).unwrap();
        let resp = InteractionResponse::text("c1", "answer");
        write_response(run_id, &resp).unwrap();

        assert!(pending_path(run_id).exists());
        assert!(response_path(run_id).exists());

        clear_interaction(run_id);

        assert!(!pending_path(run_id).exists());
        assert!(!response_path(run_id).exists());

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    #[test]
    fn test_read_request_missing_returns_none() {
        assert!(read_request("nonexistent-run-interaction").is_none());
    }

    #[test]
    fn test_take_response_missing_returns_none() {
        assert!(take_response("nonexistent-run-interaction").is_none());
    }

    // ─── InteractionRequest serde roundtrip ────────────────────────────────

    #[test]
    fn test_interaction_request_serde_roundtrip() {
        let req = InteractionRequest::tool_approval(
            "serde1",
            "bash",
            serde_json::json!({"cmd": "ls -la"}),
            "code",
        );
        let json = serde_json::to_string(&req).unwrap();
        let back: InteractionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "serde1");
        assert_eq!(back.kind, InteractionKind::ToolApproval);
        assert_eq!(back.tool_name.as_deref(), Some("bash"));
    }

    // ─── InteractionResponse serde roundtrip ───────────────────────────────

    #[test]
    fn test_interaction_response_serde_roundtrip() {
        let resp = InteractionResponse::approval("serde2", true, ApprovalScope::Session);
        let json = serde_json::to_string(&resp).unwrap();
        let back: InteractionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.request_id, "serde2");
        assert_eq!(back.approved, Some(true));
        assert_eq!(back.scope, Some(ApprovalScope::Session));
    }

    // ─── pending_path / response_path ──────────────────────────────────────

    #[test]
    fn test_pending_path_structure() {
        let path = pending_path("run-abc");
        assert!(path.to_str().unwrap().contains("run-abc"));
        assert!(path.to_str().unwrap().ends_with("pending.json"));
    }

    #[test]
    fn test_response_path_structure() {
        let path = response_path("run-abc");
        assert!(path.to_str().unwrap().contains("run-abc"));
        assert!(path.to_str().unwrap().ends_with("response.json"));
    }

    // ─── make_interaction_id edge cases ────────────────────────────────────

    #[test]
    fn test_make_interaction_id_zero() {
        assert_eq!(make_interaction_id(0, 0), "0-0");
    }

    #[test]
    fn test_make_interaction_id_large() {
        assert_eq!(make_interaction_id(999, 1000), "999-1000");
    }

    // ─── write_request/read_request with complex data ─────────────────────

    #[test]
    fn test_write_read_request_tool_approval() {
        let run_id = "test-interaction-rw-ta";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let req = InteractionRequest::tool_approval(
            "ta1",
            "bash",
            serde_json::json!({"command": "rm -rf /", "cwd": "/tmp"}),
            "code",
        );
        write_request(run_id, &req).unwrap();

        let back = read_request(run_id).unwrap();
        assert_eq!(back.kind, InteractionKind::ToolApproval);
        assert_eq!(back.tool_name.as_deref(), Some("bash"));
        assert!(back.tool_arguments.is_some());
        assert_eq!(back.options.len(), 3);

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    #[test]
    fn test_write_read_request_multiple_choice() {
        let run_id = "test-interaction-rw-mc";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let req = InteractionRequest::multiple_choice(
            "mc1",
            "Pick approach",
            vec!["Fast".into(), "Thorough".into(), "Cancel".into()],
            "plan",
        );
        write_request(run_id, &req).unwrap();

        let back = read_request(run_id).unwrap();
        assert_eq!(back.kind, InteractionKind::MultipleChoice);
        assert_eq!(back.options.len(), 3);
        assert_eq!(back.options[0], "Fast");

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    #[test]
    fn test_write_read_request_confirm() {
        let run_id = "test-interaction-rw-confirm";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let req = InteractionRequest::confirm("cf1", "Deploy to prod?", "deploy");
        write_request(run_id, &req).unwrap();

        let back = read_request(run_id).unwrap();
        assert_eq!(back.kind, InteractionKind::Confirm);
        assert_eq!(back.options, vec!["Yes", "No"]);

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    #[test]
    fn test_write_read_request_review() {
        let run_id = "test-interaction-rw-review";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let req = InteractionRequest::review(
            "rev1",
            "Architecture Review",
            "# Architecture\n\n- Component A\n- Component B",
            "plan",
        );
        write_request(run_id, &req).unwrap();

        let back = read_request(run_id).unwrap();
        assert_eq!(back.body_format, BodyFormat::Markdown);
        assert!(back.body.as_deref().unwrap().contains("Component A"));

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    // ─── write_response/take_response approval ────────────────────────────

    #[test]
    fn test_write_take_response_approval() {
        let run_id = "test-interaction-rw-approval";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let resp = InteractionResponse::approval("ap1", true, ApprovalScope::Session);
        write_response(run_id, &resp).unwrap();

        let back = take_response(run_id).unwrap();
        assert_eq!(back.approved, Some(true));
        assert_eq!(back.scope, Some(ApprovalScope::Session));

        // Should be consumed
        assert!(take_response(run_id).is_none());

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    #[test]
    fn test_write_take_response_choice() {
        let run_id = "test-interaction-rw-choice";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let resp = InteractionResponse::choice("ch1", 2);
        write_response(run_id, &resp).unwrap();

        let back = take_response(run_id).unwrap();
        assert_eq!(back.choice_index, Some(2));

        let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
    }

    // ─── response_as_choice edge cases ────────────────────────────────────

    #[test]
    fn test_response_as_choice_no_choice_index() {
        let opts = vec!["A".to_string(), "B".to_string()];
        let r = InteractionResponse::text("id", "hello");
        assert!(response_as_choice(&r, &opts).is_none());
    }

    #[test]
    fn test_response_as_choice_empty_options() {
        let opts: Vec<String> = vec![];
        let r = InteractionResponse::choice("id", 0);
        assert!(response_as_choice(&r, &opts).is_none());
    }

    // ─── InteractionRequest field defaults ────────────────────────────────

    #[test]
    fn test_free_text_request_defaults() {
        let r = InteractionRequest::free_text("ft", "prompt", "stage", true);
        assert!(r.body.is_none());
        assert_eq!(r.body_format, BodyFormat::Plain);
        assert!(r.tool_name.is_none());
        assert!(r.tool_arguments.is_none());
        assert!(r.options.is_empty());
    }

    #[test]
    fn test_multiple_choice_request_is_required() {
        let r = InteractionRequest::multiple_choice("mc", "Pick", vec!["A".into()], "stage");
        assert!(r.required);
    }

    #[test]
    fn test_confirm_request_is_required() {
        let r = InteractionRequest::confirm("c", "Sure?", "stage");
        assert!(r.required);
    }

    #[test]
    fn test_tool_approval_is_required() {
        let r = InteractionRequest::tool_approval("ta", "bash", serde_json::json!({}), "stage");
        assert!(r.required);
    }

    // ─── clear_interaction on nonexistent is safe ─────────────────────────

    #[test]
    fn test_clear_interaction_nonexistent_does_not_panic() {
        clear_interaction("nonexistent-run-clear-test");
    }

    // ─── InteractionKind serde snake_case values ──────────────────────────

    #[test]
    fn test_interaction_kind_snake_case_values() {
        assert_eq!(
            serde_json::to_string(&InteractionKind::FreeText).unwrap(),
            "\"free_text\""
        );
        assert_eq!(
            serde_json::to_string(&InteractionKind::MultipleChoice).unwrap(),
            "\"multiple_choice\""
        );
        assert_eq!(
            serde_json::to_string(&InteractionKind::ToolApproval).unwrap(),
            "\"tool_approval\""
        );
        assert_eq!(
            serde_json::to_string(&InteractionKind::Confirm).unwrap(),
            "\"confirm\""
        );
    }

    // ─── BodyFormat serde snake_case ──────────────────────────────────────

    #[test]
    fn test_body_format_snake_case_values() {
        assert_eq!(
            serde_json::to_string(&BodyFormat::Plain).unwrap(),
            "\"plain\""
        );
        assert_eq!(
            serde_json::to_string(&BodyFormat::Markdown).unwrap(),
            "\"markdown\""
        );
    }

    // ─── Full InteractionRequest serde roundtrip for each kind ────────────

    #[test]
    fn test_request_free_text_serde_roundtrip() {
        let req = InteractionRequest::free_text("ft1", "What?", "main", false);
        let json = serde_json::to_string(&req).unwrap();
        let back: InteractionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "ft1");
        assert_eq!(back.kind, InteractionKind::FreeText);
        assert!(!back.required);
        assert_eq!(back.stage_name, "main");
    }

    #[test]
    fn test_request_multiple_choice_serde_roundtrip() {
        let req = InteractionRequest::multiple_choice(
            "mc1",
            "Choose",
            vec!["A".into(), "B".into(), "C".into()],
            "plan",
        );
        let json = serde_json::to_string(&req).unwrap();
        let back: InteractionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, InteractionKind::MultipleChoice);
        assert_eq!(back.options.len(), 3);
        assert_eq!(back.options[2], "C");
    }

    #[test]
    fn test_request_confirm_serde_roundtrip() {
        let req = InteractionRequest::confirm("c1", "Proceed?", "deploy");
        let json = serde_json::to_string(&req).unwrap();
        let back: InteractionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, InteractionKind::Confirm);
        assert_eq!(back.options, vec!["Yes", "No"]);
    }

    #[test]
    fn test_request_review_serde_roundtrip() {
        let req = InteractionRequest::review("rev1", "Title", "# Body\ntext", "review");
        let json = serde_json::to_string(&req).unwrap();
        let back: InteractionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.body_format, BodyFormat::Markdown);
        assert_eq!(back.body.as_deref(), Some("# Body\ntext"));
    }

    // ─── InteractionResponse serde for all constructors ───────────────────

    #[test]
    fn test_response_text_serde_roundtrip() {
        let resp = InteractionResponse::text("t1", "my answer");
        let json = serde_json::to_string(&resp).unwrap();
        let back: InteractionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.request_id, "t1");
        assert_eq!(back.value.as_deref(), Some("my answer"));
        assert!(back.choice_index.is_none());
        assert!(back.approved.is_none());
        assert!(back.scope.is_none());
    }

    #[test]
    fn test_response_choice_serde_roundtrip() {
        let resp = InteractionResponse::choice("c1", 2);
        let json = serde_json::to_string(&resp).unwrap();
        let back: InteractionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.choice_index, Some(2));
        assert!(back.value.is_none());
    }

    #[test]
    fn test_response_approval_serde_roundtrip() {
        let resp = InteractionResponse::approval("a1", false, ApprovalScope::Session);
        let json = serde_json::to_string(&resp).unwrap();
        let back: InteractionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.approved, Some(false));
        assert_eq!(back.scope, Some(ApprovalScope::Session));
    }

    // ─── Write/read request with temp directories ─────────────────────────

    #[test]
    fn test_write_read_response_choice_roundtrip() {
        let run_id = "test-interaction-rw-choice-rt";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let resp = InteractionResponse::choice("ch-rt", 1);
        write_response(run_id, &resp).unwrap();

        let back = take_response(run_id).unwrap();
        assert_eq!(back.request_id, "ch-rt");
        assert_eq!(back.choice_index, Some(1));
        assert!(back.value.is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    // ─── clear_interaction after only writing request ─────────────────────

    #[test]
    fn test_clear_interaction_only_request() {
        let run_id = "test-interaction-clear-req-only";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let req = InteractionRequest::free_text("cr1", "prompt", "stage", true);
        write_request(run_id, &req).unwrap();
        assert!(pending_path(run_id).exists());

        clear_interaction(run_id);
        assert!(!pending_path(run_id).exists());
        assert!(!response_path(run_id).exists());

        let _ = std::fs::remove_dir_all(dir);
    }

    // ─── read_request returns None for corrupted JSON ─────────────────────

    #[test]
    fn test_read_request_corrupted_json_returns_none() {
        let run_id = "test-interaction-corrupt-req";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(pending_path(run_id), "not valid json {{{").unwrap();
        assert!(read_request(run_id).is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    // ─── take_response returns None for corrupted JSON ────────────────────

    #[test]
    fn test_take_response_corrupted_json_returns_none() {
        let run_id = "test-interaction-corrupt-resp";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(response_path(run_id), "garbage").unwrap();
        assert!(take_response(run_id).is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    // ─── request_id matching in write/read ─────────────────────────────────

    #[test]
    fn test_request_id_preserved_through_write_read() {
        let run_id = "test-interaction-reqid";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let req = InteractionRequest::tool_approval(
            "unique-req-42",
            "write_file",
            serde_json::json!({"path": "/tmp/foo"}),
            "code",
        );
        write_request(run_id, &req).unwrap();

        let back = read_request(run_id).unwrap();
        assert_eq!(back.id, "unique-req-42");

        // Write response with matching request_id
        let resp = InteractionResponse::approval("unique-req-42", true, ApprovalScope::Once);
        write_response(run_id, &resp).unwrap();

        let back_resp = take_response(run_id).unwrap();
        assert_eq!(back_resp.request_id, "unique-req-42");

        let _ = std::fs::remove_dir_all(dir);
    }

    // ─── Multiple write_request overwrites previous ───────────────────────

    #[test]
    fn test_write_request_overwrites_previous() {
        let run_id = "test-interaction-overwrite";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let req1 = InteractionRequest::free_text("first", "First?", "stage", true);
        write_request(run_id, &req1).unwrap();

        let req2 = InteractionRequest::free_text("second", "Second?", "stage", true);
        write_request(run_id, &req2).unwrap();

        let back = read_request(run_id).unwrap();
        assert_eq!(back.id, "second");
        assert_eq!(back.prompt, "Second?");

        let _ = std::fs::remove_dir_all(dir);
    }

    // ─── response_as_text with value ──────────────────────────────────────

    #[test]
    fn test_response_as_text_with_value() {
        let r = InteractionResponse::text("id", "some text value");
        assert_eq!(response_as_text(&r), "some text value");
    }

    // ─── response_approved with various states ────────────────────────────

    #[test]
    fn test_response_approved_session_scope() {
        let r = InteractionResponse::approval("id", true, ApprovalScope::Session);
        assert!(response_approved(&r));
        assert_eq!(r.scope, Some(ApprovalScope::Session));
    }

    // ─── make_interaction_id additional values ────────────────────────────

    #[test]
    fn test_make_interaction_id_various() {
        assert_eq!(make_interaction_id(1, 2), "1-2");
        assert_eq!(make_interaction_id(10, 20), "10-20");
    }
}
