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

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::runstate::{run_dir, write_meta, RunMeta, RunStatus};

// ─── Value types (moved to `leviath-core`, re-exported for compat) ────────────
//
// The plain serde value types and their pure resolver helpers now live in
// `leviath_core::interaction` so the engine in `leviath-runtime` can reference
// them without depending on the CLI. They are re-exported here so existing
// `crate::interaction::*` paths continue to resolve unchanged.
pub use leviath_core::interaction::{
    make_interaction_id, response_approved, response_as_choice, response_as_text, ApprovalScope,
    BodyFormat, InteractionKind, InteractionRequest, InteractionResponse,
};

// ─── File paths ─────────────────────────────────────────────────────────────

pub fn pending_path(run_id: &str) -> PathBuf {
    run_dir(run_id).join("pending.json")
}

pub fn response_path(run_id: &str) -> PathBuf {
    run_dir(run_id).join("response.json")
}

// ─── Write helpers (used by the dashboard / `lev respond`) ─────────────────

/// Serialize `value` to pretty JSON and atomically write it to `path` (via a
/// `.json.tmp` sibling + rename). Generic so tests can exercise the
/// serialization-failure arm directly with a value that's guaranteed to
/// fail, without needing a real (non-existent) failure mode for the
/// trivially-serializable production types that actually flow through it.
///
/// COVERAGE-CONFIRMED-ARTIFACT: this function has 3 monomorphizations
/// (`InteractionRequest`, `InteractionResponse`, and test-only
/// `PoisonSerialize`); `test_write_json_atomic_serialize_failure` drives
/// the `serde_json::to_string_pretty(value)?` error arm for real through
/// the `PoisonSerialize` instantiation (confirmed via direct HTML/JSON
/// segment inspection: that instantiation's region at this `?` shows a
/// nonzero execution count, and the file's HTML coverage report shows no
/// red/uncovered regions anywhere in this function), but `cargo-llvm-cov`'s
/// per-file region-coverage summary table still attributes the shared
/// source position to the `InteractionRequest`/`InteractionResponse`
/// instantiations (which never take that branch, since those real types
/// cannot fail to serialize) and reports it missed anyway. This is a
/// measurement artifact, not an untested branch.
fn write_json_atomic<T: serde::Serialize>(path: &std::path::Path, value: &T) -> anyhow::Result<()> {
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(value)?;
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Write an interaction request to disk (called by the worker).
pub fn write_request(run_id: &str, req: &InteractionRequest) -> anyhow::Result<()> {
    write_json_atomic(&pending_path(run_id), req)
}

/// Read the current interaction request for a run (used by the dashboard).
pub fn read_request(run_id: &str) -> Option<InteractionRequest> {
    let path = pending_path(run_id);
    let json = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&json).ok()
}

/// Write an interaction response to disk (called by the dashboard / `lev respond`).
pub fn write_response(run_id: &str, resp: &InteractionResponse) -> anyhow::Result<()> {
    write_json_atomic(&response_path(run_id), resp)
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

/// Default timeout for [`request_tool_approval_background`] in production use.
pub const TOOL_APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// Request tool approval from a background worker without a live RunMeta handle.
///
/// Reads `meta.json` from disk to update status, then polls for a response.
/// Returns `true` if approved (once or session), `false` if denied.
/// Also returns the `ApprovalScope` so callers can record session-level allows.
///
/// `timeout` is exposed (rather than hardcoded) so tests can exercise the
/// timeout/auto-deny path without a real multi-minute wait; production
/// callers should pass [`TOOL_APPROVAL_TIMEOUT`].
pub async fn request_tool_approval_background(
    run_id: &str,
    tool_name: &str,
    arguments: &serde_json::Value,
    stage_name: &str,
    timeout: Duration,
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
    let stdin = std::io::stdin();
    request_interaction_from_reader(req, &mut stdin.lock())
}

/// Same protocol as [`request_interaction_stdin`], reading from any
/// [`std::io::BufRead`] instead of hardcoding `io::stdin()`. This is the
/// actual implementation — factored out so it can be exercised in tests
/// with an in-memory reader (e.g. `Cursor<&[u8]>`) instead of blocking on
/// real stdin.
///
/// If the reader hits EOF (returns `Ok(0)`) before a valid answer is given
/// — e.g. stdin is closed/piped from `/dev/null` — returns a safe default
/// instead of looping forever: the first option for `MultipleChoice`, and
/// a denial for `Confirm`/`ToolApproval`.
pub fn request_interaction_from_reader(
    req: &InteractionRequest,
    reader: &mut dyn std::io::BufRead,
) -> InteractionResponse {
    use std::io::Write;

    println!("\n[Interaction Point: {}]", req.stage_name);

    match req.kind {
        InteractionKind::FreeText => {
            println!("{}", req.prompt);
            if !req.required {
                println!("(Press Enter to skip)");
            }
            print!("You: ");
            std::io::stdout().flush().ok();
            let mut input = String::new();
            reader.read_line(&mut input).ok();
            InteractionResponse::text(&req.id, input.trim())
        }

        InteractionKind::EditText => {
            println!("{}", req.prompt);
            let current = req.body.clone().unwrap_or_default();
            println!("--- current content ---\n{}", current);
            println!("--- enter replacement (empty line keeps current) ---");
            print!("Edit: ");
            std::io::stdout().flush().ok();
            let mut input = String::new();
            if reader.read_line(&mut input).unwrap_or(0) == 0 {
                // EOF — keep the current content unchanged.
                return InteractionResponse::text(&req.id, current);
            }
            let trimmed = input.trim();
            if trimmed.is_empty() {
                InteractionResponse::text(&req.id, current)
            } else {
                InteractionResponse::text(&req.id, trimmed)
            }
        }

        InteractionKind::MultipleChoice => {
            println!("{}", req.prompt);
            for (i, opt) in req.options.iter().enumerate() {
                println!("  [{}] {}", i + 1, opt);
            }
            loop {
                print!("Choice (1-{}): ", req.options.len());
                std::io::stdout().flush().ok();
                let mut input = String::new();
                if reader.read_line(&mut input).unwrap_or(0) == 0 {
                    return InteractionResponse::choice(&req.id, 0);
                }
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
            std::io::stdout().flush().ok();
            let mut input = String::new();
            if reader.read_line(&mut input).unwrap_or(0) == 0 {
                return InteractionResponse::approval(&req.id, false, ApprovalScope::Once);
            }
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
                std::io::stdout().flush().ok();
                let mut input = String::new();
                if reader.read_line(&mut input).unwrap_or(0) == 0 {
                    return InteractionResponse::approval(&req.id, false, ApprovalScope::Once);
                }
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
    fn test_edit_text_request_builder_seeds_body() {
        let r = InteractionRequest::edit_text("id4", "Edit this", "plan", "current text");
        assert_eq!(r.kind, InteractionKind::EditText);
        assert!(r.required);
        assert_eq!(r.body.as_deref(), Some("current text"));
        assert_eq!(r.prompt, "Edit this");
    }

    #[test]
    fn test_edit_text_kind_serde_roundtrip_snake_case() {
        let r = InteractionRequest::edit_text("id5", "p", "plan", "seed");
        let json = serde_json::to_string(&r).unwrap();
        // snake_case rename ⇒ "edit_text"
        assert!(json.contains("\"edit_text\""));
        let back: InteractionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, InteractionKind::EditText);
        assert_eq!(back.body.as_deref(), Some("seed"));
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
        let _guard = crate::runstate::isolate_runs_dir_for_test("test_write_and_read_request");
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
    fn test_write_request_rename_fails_when_target_is_a_directory() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_write_request_rename_fails_when_target_is_a_directory",
        );
        let run_id = "test-write-request-target-is-dir";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        // Pre-create the target path as a directory: the tmp-file write
        // succeeds, but `fs::rename` onto an existing directory fails,
        // exercising `write_request`'s rename `?`.
        std::fs::create_dir_all(pending_path(run_id)).unwrap();

        let req = InteractionRequest::free_text("rw1", "What now?", "plan", true);
        let result = write_request(run_id, &req);
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_request_tmp_write_fails_when_target_is_a_directory() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_write_request_tmp_write_fails_when_target_is_a_directory",
        );
        let run_id = "test-write-request-tmp-is-dir";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        // Pre-create the *tmp* path (not the final path, covered by
        // `test_write_request_rename_fails_when_target_is_a_directory` above)
        // as a directory: `std::fs::write(&tmp, &json)` itself fails with
        // EISDIR before `fs::rename` is ever reached, exercising
        // `write_request`'s tmp-file-write `?` -- a distinct branch from the
        // rename failure, since a well-formed JSON body is never itself
        // capable of making `serde_json::to_string_pretty` fail.
        std::fs::create_dir_all(pending_path(run_id).with_extension("json.tmp")).unwrap();

        let req = InteractionRequest::free_text("rw1", "What now?", "plan", true);
        let result = write_request(run_id, &req);
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_json_atomic_serialize_failure() {
        // `InteractionRequest`/`InteractionResponse` are trivially
        // serializable and can never actually fail `to_string_pretty`, so
        // this drives `write_json_atomic`'s error `?` directly with a
        // value whose `Serialize` impl always errs (see
        // `crate::test_support::PoisonSerialize`), exercising the real
        // production error-propagation path honestly.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("poison.json");
        let result = write_json_atomic(&path, &crate::test_support::PoisonSerialize);
        assert!(result.is_err());
        assert!(!path.exists());
    }

    #[test]
    fn test_write_and_read_response() {
        let _guard = crate::runstate::isolate_runs_dir_for_test("test_write_and_read_response");
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
    fn test_write_response_rename_fails_when_target_is_a_directory() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_write_response_rename_fails_when_target_is_a_directory",
        );
        let run_id = "test-write-response-target-is-dir";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        // Pre-create the target path as a directory: the tmp-file write
        // succeeds, but `fs::rename` onto an existing directory fails,
        // exercising `write_response`'s rename `?`.
        std::fs::create_dir_all(response_path(run_id)).unwrap();

        let resp = InteractionResponse::text("rw2", "my answer");
        let result = write_response(run_id, &resp);
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_write_response_tmp_write_fails_when_target_is_a_directory() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_write_response_tmp_write_fails_when_target_is_a_directory",
        );
        let run_id = "test-write-response-tmp-is-dir";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        // See `test_write_request_tmp_write_fails_when_target_is_a_directory`
        // -- same distinction, for `write_response`'s own tmp-file write.
        std::fs::create_dir_all(response_path(run_id).with_extension("json.tmp")).unwrap();

        let resp = InteractionResponse::text("rw2", "my answer");
        let result = write_response(run_id, &resp);
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_clear_interaction() {
        let _guard = crate::runstate::isolate_runs_dir_for_test("test_clear_interaction");
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
        let _guard =
            crate::runstate::isolate_runs_dir_for_test("test_write_read_request_tool_approval");
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
        let _guard =
            crate::runstate::isolate_runs_dir_for_test("test_write_read_request_multiple_choice");
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
        let _guard = crate::runstate::isolate_runs_dir_for_test("test_write_read_request_confirm");
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
        let _guard = crate::runstate::isolate_runs_dir_for_test("test_write_read_request_review");
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
        let _guard =
            crate::runstate::isolate_runs_dir_for_test("test_write_take_response_approval");
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
        let _guard = crate::runstate::isolate_runs_dir_for_test("test_write_take_response_choice");
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
        let _guard =
            crate::runstate::isolate_runs_dir_for_test("test_write_read_response_choice_roundtrip");
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
        let _guard =
            crate::runstate::isolate_runs_dir_for_test("test_clear_interaction_only_request");
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
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_read_request_corrupted_json_returns_none",
        );
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
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_take_response_corrupted_json_returns_none",
        );
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
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_request_id_preserved_through_write_read",
        );
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
        let _guard =
            crate::runstate::isolate_runs_dir_for_test("test_write_request_overwrites_previous");
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

    // ─── default_true() via serde deserialization ─────────────────────────
    // Deserializing a request that lacks `required` exercises default_true().

    #[test]
    fn test_default_true_via_serde_missing_required_field() {
        // JSON without `required` — should default to true via default_true()
        let json = r#"{
            "id": "dt1",
            "kind": "free_text",
            "prompt": "test prompt",
            "stage_name": "stage"
        }"#;
        let req: InteractionRequest = serde_json::from_str(json).unwrap();
        assert!(req.required);
    }

    // ─── request_interaction (synchronous) ───────────────────────────────
    // Spawn a thread that writes the response after a short delay.

    #[test]
    fn test_request_interaction_sync_success() {
        let _guard =
            crate::runstate::isolate_runs_dir_for_test("test_request_interaction_sync_success");
        let run_id = "test-sync-request-ok";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        // Create meta.json so write_meta succeeds
        let mut meta = crate::runstate::RunMeta::new(
            run_id.to_string(),
            "agent".to_string(),
            "/tmp/agent.toml".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        crate::runstate::create_run(&meta).unwrap();

        let req_id = "sync-req-1".to_string();
        let run_id_clone = run_id.to_string();
        let req_id_clone = req_id.clone();

        // Spawn thread that writes the response after 150ms
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let resp = InteractionResponse::text(&req_id_clone, "the answer");
            write_response(&run_id_clone, &resp).ok();
        });

        let req = InteractionRequest::free_text(&req_id, "What?", "plan", true);
        let result = request_interaction(run_id, &mut meta, req, Some(Duration::from_secs(5)));
        assert!(result.is_ok());
        let resp = result.unwrap();
        assert_eq!(resp.value.as_deref(), Some("the answer"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_request_interaction_sync_no_timeout_waits_indefinitely_for_response() {
        // `timeout: None` means the poll loop never checks elapsed time — cover
        // that branch explicitly so it isn't left to the `Some(t)` tests.
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_request_interaction_sync_no_timeout_waits_indefinitely_for_response",
        );
        let run_id = "test-sync-request-no-timeout";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let mut meta = crate::runstate::RunMeta::new(
            run_id.to_string(),
            "agent".to_string(),
            "/tmp/agent.toml".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        crate::runstate::create_run(&meta).unwrap();

        let req_id = "sync-no-timeout-1".to_string();
        let run_id_clone = run_id.to_string();
        let req_id_clone = req_id.clone();

        // Spawn thread that writes the response after a couple of poll cycles.
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let resp = InteractionResponse::text(&req_id_clone, "no-timeout answer");
            write_response(&run_id_clone, &resp).ok();
        });

        let req = InteractionRequest::free_text(&req_id, "What?", "plan", true);
        let result = request_interaction(run_id, &mut meta, req, None);
        assert!(result.is_ok());
        let resp = result.unwrap();
        assert_eq!(resp.value.as_deref(), Some("no-timeout answer"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_request_interaction_sync_timeout() {
        let _guard =
            crate::runstate::isolate_runs_dir_for_test("test_request_interaction_sync_timeout");
        let run_id = "test-sync-request-timeout";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let mut meta = crate::runstate::RunMeta::new(
            run_id.to_string(),
            "agent".to_string(),
            "/tmp/agent.toml".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        crate::runstate::create_run(&meta).unwrap();

        let req = InteractionRequest::free_text("sync-timeout", "What?", "plan", true);
        // Timeout of 200ms — no response will be written
        let result = request_interaction(run_id, &mut meta, req, Some(Duration::from_millis(200)));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timed out"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_request_interaction_sync_not_required_status() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_request_interaction_sync_not_required_status",
        );
        let run_id = "test-sync-request-not-required";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let mut meta = crate::runstate::RunMeta::new(
            run_id.to_string(),
            "agent".to_string(),
            "/tmp/agent.toml".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        crate::runstate::create_run(&meta).unwrap();

        let run_id_clone = run_id.to_string();
        // Spawn thread that writes the response immediately
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let resp = InteractionResponse::text("not-req-1", "ok");
            write_response(&run_id_clone, &resp).ok();
        });

        // required: false → CompleteInteractive status branch
        let req = InteractionRequest::free_text("not-req-1", "Optional?", "plan", false);
        let result = request_interaction(run_id, &mut meta, req, Some(Duration::from_secs(5)));
        assert!(result.is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_request_interaction_sync_stale_response_then_correct() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_request_interaction_sync_stale_response_then_correct",
        );
        let run_id = "test-sync-stale-then-correct";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let mut meta = crate::runstate::RunMeta::new(
            run_id.to_string(),
            "agent".to_string(),
            "/tmp/agent.toml".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        crate::runstate::create_run(&meta).unwrap();

        let run_id_clone = run_id.to_string();
        // Write a stale response first (different request_id), then the correct one
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(120));
            // Stale response
            let stale = InteractionResponse::text("other-req", "stale answer");
            write_response(&run_id_clone, &stale).ok();
            // Give time for it to be consumed, then write correct one
            std::thread::sleep(std::time::Duration::from_millis(150));
            let correct = InteractionResponse::text("target-req", "correct answer");
            write_response(&run_id_clone, &correct).ok();
        });

        let req = InteractionRequest::free_text("target-req", "What?", "plan", true);
        let result = request_interaction(run_id, &mut meta, req, Some(Duration::from_secs(5)));
        assert!(result.is_ok());
        let resp = result.unwrap();
        assert_eq!(resp.value.as_deref(), Some("correct answer"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_request_interaction_sync_write_request_fails_when_run_dir_missing() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_request_interaction_sync_write_request_fails_when_run_dir_missing",
        );
        let run_id = "test-sync-request-no-dir";
        // Deliberately skip creating the run directory, so `write_request`'s
        // tmp-file write fails immediately, exercising this function's own
        // `write_request(...)?` propagation.
        let mut meta = crate::runstate::RunMeta::new(
            run_id.to_string(),
            "agent".to_string(),
            "/tmp/agent.toml".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );

        let req = InteractionRequest::free_text("no-dir-req", "What?", "plan", true);
        let result = request_interaction(run_id, &mut meta, req, Some(Duration::from_secs(5)));
        assert!(result.is_err());
    }

    #[test]
    fn test_request_interaction_sync_write_meta_fails_when_target_is_a_directory() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_request_interaction_sync_write_meta_fails_when_target_is_a_directory",
        );
        let run_id = "test-sync-request-meta-target-is-dir";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        // Pre-create meta.json as a directory: write_request succeeds (it
        // touches a different filename), but the subsequent `write_meta`'s
        // rename onto "meta.json" fails, exercising this function's own
        // `write_meta(...)?` propagation.
        std::fs::create_dir_all(dir.join("meta.json")).unwrap();

        let mut meta = crate::runstate::RunMeta::new(
            run_id.to_string(),
            "agent".to_string(),
            "/tmp/agent.toml".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );

        let req = InteractionRequest::free_text("meta-fail-req", "What?", "plan", true);
        let result = request_interaction(run_id, &mut meta, req, Some(Duration::from_secs(5)));
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── request_interaction_async ────────────────────────────────────────

    #[tokio::test]
    async fn test_request_interaction_async_success() {
        let _guard =
            crate::runstate::isolate_runs_dir_for_test("test_request_interaction_async_success");
        let run_id = "test-async-request-ok";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let mut meta = crate::runstate::RunMeta::new(
            run_id.to_string(),
            "agent".to_string(),
            "/tmp/agent.toml".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        crate::runstate::create_run(&meta).unwrap();

        let run_id_clone = run_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let resp = InteractionResponse::text("async-req-1", "async answer");
            write_response(&run_id_clone, &resp).ok();
        });

        let req = InteractionRequest::free_text("async-req-1", "Async?", "plan", true);
        let result =
            request_interaction_async(run_id, &mut meta, req, Some(Duration::from_secs(5))).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().value.as_deref(), Some("async answer"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_request_interaction_async_timeout() {
        let _guard =
            crate::runstate::isolate_runs_dir_for_test("test_request_interaction_async_timeout");
        let run_id = "test-async-request-timeout";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let mut meta = crate::runstate::RunMeta::new(
            run_id.to_string(),
            "agent".to_string(),
            "/tmp/agent.toml".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        crate::runstate::create_run(&meta).unwrap();

        let req = InteractionRequest::free_text("async-timeout", "Async?", "plan", true);
        let result =
            request_interaction_async(run_id, &mut meta, req, Some(Duration::from_millis(200)))
                .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timed out"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_request_interaction_async_not_required() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_request_interaction_async_not_required",
        );
        let run_id = "test-async-request-not-req";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let mut meta = crate::runstate::RunMeta::new(
            run_id.to_string(),
            "agent".to_string(),
            "/tmp/agent.toml".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        crate::runstate::create_run(&meta).unwrap();

        let run_id_clone = run_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let resp = InteractionResponse::text("async-not-req", "ok");
            write_response(&run_id_clone, &resp).ok();
        });

        // required: false → CompleteInteractive status branch
        let req = InteractionRequest::free_text("async-not-req", "Optional?", "plan", false);
        let result =
            request_interaction_async(run_id, &mut meta, req, Some(Duration::from_secs(5))).await;
        assert!(result.is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_request_interaction_async_write_meta_fails_when_target_is_a_directory() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_request_interaction_async_write_meta_fails_when_target_is_a_directory",
        );
        let run_id = "test-async-request-meta-target-is-dir";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        // Pre-create meta.json as a directory: write_request succeeds (it
        // touches a different filename), but the subsequent `write_meta`'s
        // rename onto "meta.json" fails, exercising this function's own
        // `write_meta(...)?` propagation.
        std::fs::create_dir_all(dir.join("meta.json")).unwrap();

        let mut meta = crate::runstate::RunMeta::new(
            run_id.to_string(),
            "agent".to_string(),
            "/tmp/agent.toml".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );

        let req = InteractionRequest::free_text("async-meta-fail-req", "What?", "plan", true);
        let result =
            request_interaction_async(run_id, &mut meta, req, Some(Duration::from_secs(5))).await;
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_request_interaction_async_stale_then_correct() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_request_interaction_async_stale_then_correct",
        );
        let run_id = "test-async-stale-then-correct";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let mut meta = crate::runstate::RunMeta::new(
            run_id.to_string(),
            "agent".to_string(),
            "/tmp/agent.toml".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        crate::runstate::create_run(&meta).unwrap();

        let run_id_clone = run_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            // Stale response
            let stale = InteractionResponse::text("wrong-id", "stale");
            write_response(&run_id_clone, &stale).ok();
            tokio::time::sleep(Duration::from_millis(200)).await;
            // Correct response
            let correct = InteractionResponse::text("async-stale-target", "correct");
            write_response(&run_id_clone, &correct).ok();
        });

        let req = InteractionRequest::free_text("async-stale-target", "Async?", "plan", true);
        let result =
            request_interaction_async(run_id, &mut meta, req, Some(Duration::from_secs(5))).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().value.as_deref(), Some("correct"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── request_interaction_bg_review ────────────────────────────────────

    #[tokio::test]
    async fn test_request_interaction_bg_review_responds() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_request_interaction_bg_review_responds",
        );
        let run_id = "test-bg-review-ok";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        // Create meta so the meta-update path succeeds
        let meta = crate::runstate::RunMeta::new(
            run_id.to_string(),
            "agent".to_string(),
            "/tmp/agent.toml".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        crate::runstate::create_run(&meta).unwrap();

        let run_id_clone = run_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let resp = InteractionResponse::text("bg-rev-1", "reviewed");
            write_response(&run_id_clone, &resp).ok();
        });

        let req =
            InteractionRequest::review("bg-rev-1", "Review this", "# Plan\n\nDetails here", "plan");
        let resp = request_interaction_bg_review(run_id, req).await;
        assert_eq!(resp.value.as_deref(), Some("reviewed"));
        assert_eq!(resp.request_id, "bg-rev-1");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_request_interaction_bg_review_no_meta() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_request_interaction_bg_review_no_meta",
        );
        // bg_review should work even when meta.json doesn't exist
        let run_id = "test-bg-review-no-meta";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        // No meta.json created

        let run_id_clone = run_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let resp = InteractionResponse::text("bg-rev-no-meta", "answer");
            write_response(&run_id_clone, &resp).ok();
        });

        let req = InteractionRequest::free_text("bg-rev-no-meta", "Review?", "plan", true);
        let resp = request_interaction_bg_review(run_id, req).await;
        assert_eq!(resp.request_id, "bg-rev-no-meta");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_request_interaction_bg_review_stale_then_correct() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_request_interaction_bg_review_stale_then_correct",
        );
        let run_id = "test-bg-review-stale";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let run_id_clone = run_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            // Stale response
            let stale = InteractionResponse::text("wrong", "stale");
            write_response(&run_id_clone, &stale).ok();
            tokio::time::sleep(Duration::from_millis(200)).await;
            // Correct response
            let correct = InteractionResponse::text("bg-rev-target", "ok");
            write_response(&run_id_clone, &correct).ok();
        });

        let req = InteractionRequest::free_text("bg-rev-target", "Review?", "plan", true);
        let resp = request_interaction_bg_review(run_id, req).await;
        assert_eq!(resp.value.as_deref(), Some("ok"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── request_tool_approval_background ────────────────────────────────

    #[tokio::test]
    async fn test_request_tool_approval_background_approved() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_request_tool_approval_background_approved",
        );
        let run_id = "test-tool-approval-bg-ok";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let meta = crate::runstate::RunMeta::new(
            run_id.to_string(),
            "agent".to_string(),
            "/tmp/agent.toml".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        crate::runstate::create_run(&meta).unwrap();

        // Calculate the expected request ID (same hash as the function uses)
        let tool_name = "bash";
        let hash = tool_name
            .bytes()
            .fold(0usize, |a, b| a.wrapping_add(b as usize));
        let req_id = make_interaction_id(hash, 0);

        let resp_path = response_path(run_id);
        let req_id_clone = req_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let resp = InteractionResponse::approval(&req_id_clone, true, ApprovalScope::Once);
            let json = serde_json::to_string_pretty(&resp).unwrap();
            std::fs::write(&resp_path, json).ok();
        });

        let args = serde_json::json!({"command": "ls"});
        let (approved, scope) = request_tool_approval_background(
            run_id,
            tool_name,
            &args,
            "code",
            Duration::from_secs(10),
        )
        .await;
        assert!(approved);
        assert_eq!(scope, ApprovalScope::Once);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_request_tool_approval_background_denied() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_request_tool_approval_background_denied",
        );
        let run_id = "test-tool-approval-bg-denied";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        // No meta.json — exercises the else branch
        let tool_name = "write_file";
        let hash = tool_name
            .bytes()
            .fold(0usize, |a, b| a.wrapping_add(b as usize));
        let req_id = make_interaction_id(hash, 0);

        let resp_path = response_path(run_id);
        let req_id_clone = req_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let resp = InteractionResponse::approval(&req_id_clone, false, ApprovalScope::Once);
            let json = serde_json::to_string_pretty(&resp).unwrap();
            std::fs::write(&resp_path, json).ok();
        });

        let args = serde_json::json!({"path": "/tmp/f.txt"});
        let (approved, scope) = request_tool_approval_background(
            run_id,
            tool_name,
            &args,
            "code",
            Duration::from_secs(10),
        )
        .await;
        assert!(!approved);
        assert_eq!(scope, ApprovalScope::Once);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_request_tool_approval_background_session_scope() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_request_tool_approval_background_session_scope",
        );
        let run_id = "test-tool-approval-bg-session";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let meta = crate::runstate::RunMeta::new(
            run_id.to_string(),
            "agent".to_string(),
            "/tmp/agent.toml".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        crate::runstate::create_run(&meta).unwrap();

        let tool_name = "edit_file";
        let hash = tool_name
            .bytes()
            .fold(0usize, |a, b| a.wrapping_add(b as usize));
        let req_id = make_interaction_id(hash, 0);

        // Capture the response path NOW (before spawning) so the spawned task
        // doesn't re-read LEVIATH_RUNS_DIR at execution time — a concurrent test
        // in commands/run/mod.rs temporarily sets LEVIATH_RUNS_DIR to a
        // read-only dir, which would silently break write_response.
        let resp_path = response_path(run_id);
        let req_id_clone = req_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let resp = InteractionResponse::approval(&req_id_clone, true, ApprovalScope::Session);
            let json = serde_json::to_string_pretty(&resp).unwrap();
            std::fs::write(&resp_path, json).ok();
        });

        let args = serde_json::json!({});
        let (approved, scope) = request_tool_approval_background(
            run_id,
            tool_name,
            &args,
            "impl",
            Duration::from_secs(10),
        )
        .await;
        assert!(approved);
        assert_eq!(scope, ApprovalScope::Session);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_request_tool_approval_background_stale_then_correct() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_request_tool_approval_background_stale_then_correct",
        );
        let run_id = "test-tool-approval-bg-stale";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let tool_name = "bash";
        let hash = tool_name
            .bytes()
            .fold(0usize, |a, b| a.wrapping_add(b as usize));
        let req_id = make_interaction_id(hash, 0);

        let resp_path = response_path(run_id);
        let req_id_clone = req_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            // Write a stale response first
            let stale = InteractionResponse::approval("wrong-id", true, ApprovalScope::Once);
            let json = serde_json::to_string_pretty(&stale).unwrap();
            std::fs::write(&resp_path, json).ok();
            tokio::time::sleep(Duration::from_millis(200)).await;
            // Write the correct response
            let correct = InteractionResponse::approval(&req_id_clone, false, ApprovalScope::Once);
            let json = serde_json::to_string_pretty(&correct).unwrap();
            std::fs::write(&resp_path, json).ok();
        });

        let args = serde_json::json!({"command": "rm -rf"});
        let (approved, _scope) = request_tool_approval_background(
            run_id,
            tool_name,
            &args,
            "code",
            Duration::from_secs(10),
        )
        .await;
        assert!(!approved);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_request_tool_approval_background_timeout_auto_denies() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_request_tool_approval_background_timeout_auto_denies",
        );
        let run_id = "test-tool-approval-timeout";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();

        let meta = crate::runstate::RunMeta::new(
            run_id.to_string(),
            "agent".to_string(),
            "/tmp/agent.toml".to_string(),
            "task".to_string(),
            None,
            "/tmp".to_string(),
            1,
        );
        crate::runstate::create_run(&meta).unwrap();

        // No responder — the short timeout should fire the auto-deny path.
        let args = serde_json::json!({"command": "rm -rf /"});
        let (approved, scope) = request_tool_approval_background(
            run_id,
            "bash",
            &args,
            "code",
            Duration::from_millis(150),
        )
        .await;
        assert!(!approved);
        assert_eq!(scope, ApprovalScope::Once);

        // Status should be restored to Running (not left stuck WaitingInput).
        let meta_after = crate::runstate::read_meta(run_id).unwrap();
        assert_eq!(meta_after.status, RunStatus::Running);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_request_tool_approval_background_timeout_with_no_meta_file() {
        let _guard = crate::runstate::isolate_runs_dir_for_test(
            "test_request_tool_approval_background_timeout_with_no_meta_file",
        );
        let run_id = "test-tool-approval-timeout-no-meta";
        let dir = crate::runstate::run_dir(run_id);
        // Deliberately skip `create_run` — no meta.json ever exists, so every
        // `read_meta` call (including the one on the timeout path) fails,
        // exercising the `if let Ok(...)` else arm at the timeout branch.
        std::fs::create_dir_all(&dir).unwrap();

        let args = serde_json::json!({"command": "rm -rf /"});
        let (approved, scope) = request_tool_approval_background(
            run_id,
            "bash",
            &args,
            "code",
            Duration::from_millis(150),
        )
        .await;
        assert!(!approved);
        assert_eq!(scope, ApprovalScope::Once);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── request_interaction_from_reader (mocked stdin) ────────────────────

    use std::io::Cursor;

    fn reader_from(input: &str) -> Cursor<Vec<u8>> {
        Cursor::new(input.as_bytes().to_vec())
    }

    #[test]
    fn stdin_free_text_reads_trimmed_line() {
        let req = InteractionRequest::free_text("ft1", "What's up?", "plan", true);
        let mut reader = reader_from("  hello world  \n");
        let resp = request_interaction_from_reader(&req, &mut reader);
        assert_eq!(resp.value.as_deref(), Some("hello world"));
        assert_eq!(resp.request_id, "ft1");
    }

    #[test]
    fn stdin_free_text_not_required_shows_skip_hint_and_accepts_empty() {
        let req = InteractionRequest::free_text("ft2", "Optional?", "plan", false);
        let mut reader = reader_from("\n");
        let resp = request_interaction_from_reader(&req, &mut reader);
        assert_eq!(resp.value.as_deref(), Some(""));
    }

    #[test]
    fn stdin_free_text_eof_yields_empty_answer() {
        let req = InteractionRequest::free_text("ft3", "Q?", "plan", true);
        let mut reader = reader_from(""); // immediate EOF, no newline
        let resp = request_interaction_from_reader(&req, &mut reader);
        assert_eq!(resp.value.as_deref(), Some(""));
    }

    #[test]
    fn stdin_edit_text_replacement_line_replaces_body() {
        let req = InteractionRequest::edit_text("et1", "Edit", "plan", "old content");
        let mut reader = reader_from("new content\n");
        let resp = request_interaction_from_reader(&req, &mut reader);
        assert_eq!(resp.value.as_deref(), Some("new content"));
        assert_eq!(resp.request_id, "et1");
    }

    #[test]
    fn stdin_edit_text_empty_line_keeps_body() {
        let req = InteractionRequest::edit_text("et2", "Edit", "plan", "keep me");
        let mut reader = reader_from("\n");
        let resp = request_interaction_from_reader(&req, &mut reader);
        assert_eq!(resp.value.as_deref(), Some("keep me"));
    }

    #[test]
    fn stdin_edit_text_eof_keeps_body() {
        let req = InteractionRequest::edit_text("et3", "Edit", "plan", "unchanged");
        let mut reader = reader_from(""); // immediate EOF
        let resp = request_interaction_from_reader(&req, &mut reader);
        assert_eq!(resp.value.as_deref(), Some("unchanged"));
    }

    #[test]
    fn test_edit_text_request_ipc_roundtrip() {
        let _guard =
            crate::runstate::isolate_runs_dir_for_test("test_edit_text_request_ipc_roundtrip");
        let run_id = "edit-ipc-run";
        let dir = crate::runstate::run_dir(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        let req = InteractionRequest::edit_text("er1", "Edit the plan", "plan", "line 1\nline 2");
        write_request(run_id, &req).unwrap();
        let back = read_request(run_id).expect("request should round-trip");
        assert_eq!(back.kind, InteractionKind::EditText);
        assert_eq!(back.body.as_deref(), Some("line 1\nline 2"));
        assert_eq!(back.id, "er1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stdin_multiple_choice_valid_first_try() {
        let req = InteractionRequest::multiple_choice(
            "mc1",
            "Pick",
            vec!["A".into(), "B".into(), "C".into()],
            "plan",
        );
        let mut reader = reader_from("2\n");
        let resp = request_interaction_from_reader(&req, &mut reader);
        assert_eq!(resp.choice_index, Some(1));
    }

    #[test]
    fn stdin_multiple_choice_retries_on_invalid_input() {
        let req = InteractionRequest::multiple_choice(
            "mc2",
            "Pick",
            vec!["A".into(), "B".into()],
            "plan",
        );
        // "not a number", then out-of-range "9", then valid "1"
        let mut reader = reader_from("not a number\n9\n1\n");
        let resp = request_interaction_from_reader(&req, &mut reader);
        assert_eq!(resp.choice_index, Some(0));
    }

    #[test]
    fn stdin_multiple_choice_rejects_zero() {
        let req = InteractionRequest::multiple_choice(
            "mc3",
            "Pick",
            vec!["A".into(), "B".into()],
            "plan",
        );
        let mut reader = reader_from("0\n2\n");
        let resp = request_interaction_from_reader(&req, &mut reader);
        assert_eq!(resp.choice_index, Some(1));
    }

    #[test]
    fn stdin_multiple_choice_eof_defaults_to_first_option() {
        let req = InteractionRequest::multiple_choice(
            "mc4",
            "Pick",
            vec!["A".into(), "B".into()],
            "plan",
        );
        let mut reader = reader_from(""); // EOF before any answer
        let resp = request_interaction_from_reader(&req, &mut reader);
        assert_eq!(resp.choice_index, Some(0));
    }

    #[test]
    fn stdin_multiple_choice_eof_after_invalid_input_defaults_to_first_option() {
        let req = InteractionRequest::multiple_choice(
            "mc5",
            "Pick",
            vec!["A".into(), "B".into()],
            "plan",
        );
        // One invalid line, then EOF (no trailing newline / more input)
        let mut reader = reader_from("garbage\n");
        let resp = request_interaction_from_reader(&req, &mut reader);
        assert_eq!(resp.choice_index, Some(0));
    }

    #[test]
    fn stdin_confirm_yes_variants() {
        for input in ["y\n", "yes\n", "Y\n", "YES\n"] {
            let req = InteractionRequest::confirm("cf1", "Sure?", "plan");
            let mut reader = reader_from(input);
            let resp = request_interaction_from_reader(&req, &mut reader);
            assert_eq!(resp.approved, Some(true));
        }
    }

    #[test]
    fn stdin_confirm_no_variants() {
        for input in ["n\n", "no\n", "N\n", "NO\n"] {
            let req = InteractionRequest::confirm("cf2", "Sure?", "plan");
            let mut reader = reader_from(input);
            let resp = request_interaction_from_reader(&req, &mut reader);
            assert_eq!(resp.approved, Some(false));
        }
    }

    #[test]
    fn stdin_confirm_retries_on_invalid_input() {
        let req = InteractionRequest::confirm("cf3", "Sure?", "plan");
        let mut reader = reader_from("maybe\nwhat\ny\n");
        let resp = request_interaction_from_reader(&req, &mut reader);
        assert_eq!(resp.approved, Some(true));
    }

    #[test]
    fn stdin_confirm_eof_defaults_to_no() {
        let req = InteractionRequest::confirm("cf4", "Sure?", "plan");
        let mut reader = reader_from("");
        let resp = request_interaction_from_reader(&req, &mut reader);
        assert_eq!(resp.approved, Some(false));
        assert_eq!(resp.scope, Some(ApprovalScope::Once));
    }

    #[test]
    fn stdin_tool_approval_allow_once() {
        let req = InteractionRequest::tool_approval(
            "ta1",
            "bash",
            serde_json::json!({"command": "ls"}),
            "code",
        );
        let mut reader = reader_from("1\n");
        let resp = request_interaction_from_reader(&req, &mut reader);
        assert_eq!(resp.approved, Some(true));
        assert_eq!(resp.scope, Some(ApprovalScope::Once));
    }

    #[test]
    fn stdin_tool_approval_allow_session() {
        let req = InteractionRequest::tool_approval(
            "ta2",
            "bash",
            serde_json::json!({"command": "ls"}),
            "code",
        );
        let mut reader = reader_from("2\n");
        let resp = request_interaction_from_reader(&req, &mut reader);
        assert_eq!(resp.approved, Some(true));
        assert_eq!(resp.scope, Some(ApprovalScope::Session));
    }

    #[test]
    fn stdin_tool_approval_deny() {
        let req = InteractionRequest::tool_approval(
            "ta3",
            "bash",
            serde_json::json!({"command": "rm -rf /"}),
            "code",
        );
        let mut reader = reader_from("3\n");
        let resp = request_interaction_from_reader(&req, &mut reader);
        assert_eq!(resp.approved, Some(false));
        assert_eq!(resp.scope, Some(ApprovalScope::Once));
    }

    #[test]
    fn stdin_tool_approval_retries_on_invalid_input() {
        let req = InteractionRequest::tool_approval(
            "ta4",
            "write_file",
            serde_json::json!({"path": "x.txt"}),
            "code",
        );
        let mut reader = reader_from("nope\n5\n2\n");
        let resp = request_interaction_from_reader(&req, &mut reader);
        assert_eq!(resp.approved, Some(true));
        assert_eq!(resp.scope, Some(ApprovalScope::Session));
    }

    #[test]
    fn stdin_tool_approval_eof_defaults_to_deny() {
        let req = InteractionRequest::tool_approval(
            "ta5",
            "bash",
            serde_json::json!({"command": "rm -rf /"}),
            "code",
        );
        let mut reader = reader_from("");
        let resp = request_interaction_from_reader(&req, &mut reader);
        assert_eq!(resp.approved, Some(false));
    }

    #[test]
    fn stdin_tool_approval_without_arguments_still_prompts() {
        // tool_arguments is always Some(..) via the constructor, but exercise
        // the tool_name-present/arguments-present printing path explicitly.
        let req =
            InteractionRequest::tool_approval("ta6", "read_file", serde_json::json!({}), "code");
        assert!(req.tool_name.is_some());
        assert!(req.tool_arguments.is_some());
        let mut reader = reader_from("1\n");
        let resp = request_interaction_from_reader(&req, &mut reader);
        assert_eq!(resp.approved, Some(true));
    }

    #[test]
    fn stdin_tool_approval_with_no_tool_name_or_arguments_skips_that_printing() {
        // The `tool_approval()` builder always sets tool_name/tool_arguments,
        // but the ToolApproval branch itself tolerates either being absent
        // (e.g. a hand-built request) — exercise that fallback path directly.
        let req = InteractionRequest {
            id: "ta7".to_string(),
            kind: InteractionKind::ToolApproval,
            prompt: "Allow this?".to_string(),
            options: vec!["Allow once".into(), "Allow session".into(), "Deny".into()],
            tool_name: None,
            tool_arguments: None,
            required: true,
            stage_name: "code".to_string(),
            body: None,
            body_format: BodyFormat::Plain,
        };
        let mut reader = reader_from("3\n");
        let resp = request_interaction_from_reader(&req, &mut reader);
        assert_eq!(resp.approved, Some(false));
    }

    #[test]
    fn stdin_tool_approval_with_tool_name_but_no_arguments_skips_arguments_printing() {
        // tool_name is present but tool_arguments is None (e.g. a hand-built
        // request) — the tool-name line should still print, but the nested
        // `if let Some(ref args) = req.tool_arguments` block is skipped.
        let req = InteractionRequest {
            id: "ta8".to_string(),
            kind: InteractionKind::ToolApproval,
            prompt: "Allow this?".to_string(),
            options: vec!["Allow once".into(), "Allow session".into(), "Deny".into()],
            tool_name: Some("read_file".to_string()),
            tool_arguments: None,
            required: true,
            stage_name: "code".to_string(),
            body: None,
            body_format: BodyFormat::Plain,
        };
        let mut reader = reader_from("1\n");
        let resp = request_interaction_from_reader(&req, &mut reader);
        assert_eq!(resp.approved, Some(true));
    }

    #[test]
    fn stdin_request_delegates_to_real_stdin_wrapper_type_compiles() {
        // request_interaction_stdin() itself reads real stdin and can't be
        // safely called in a test (would block). This just confirms the
        // public wrapper still exists with the expected signature by
        // referencing it as a value, without calling it.
        let _f: fn(&InteractionRequest) -> InteractionResponse = request_interaction_stdin;
    }

    #[cfg(unix)]
    #[test]
    fn stdin_request_interaction_stdin_with_dev_null_exercises_wrapper_body() {
        use std::fs::File;
        use std::os::unix::io::AsRawFd;

        let devnull = File::open("/dev/null").unwrap();
        let devnull_fd = devnull.as_raw_fd();
        let old_stdin = unsafe { libc::dup(0) };
        unsafe { libc::dup2(devnull_fd, 0) };
        drop(devnull);

        let req = InteractionRequest::free_text("stdin-wrap-test", "Q?", "stage", false);
        let resp = request_interaction_stdin(&req);

        unsafe { libc::dup2(old_stdin, 0) };
        unsafe { libc::close(old_stdin) };

        assert_eq!(resp.request_id, "stdin-wrap-test");
    }
}
