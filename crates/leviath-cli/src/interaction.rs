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
}
