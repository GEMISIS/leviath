//! Agent-initiated dynamic interaction tools: `present_for_review`,
//! `ask_user_text`, `ask_user_choice`, `ask_user_confirm`, `edit_document`.
//!
//! Unlike `interaction_points` (declared statically in a blueprint and
//! always fired), these are ordinary tool calls the model makes on its own
//! judgment, mid-reasoning. Both the background worker (file-based IPC) and
//! the foreground (stdin) run modes need to intercept these tool names
//! before they ever reach the generic tool registry - this module holds
//! that shared logic behind an [`InteractionBackend`] trait so it can be
//! unit tested with a mock instead of only living inside untestable
//! closures.

use async_trait::async_trait;

use leviath_core::interaction::{ApprovalScope, InteractionRequest, InteractionResponse};
use leviath_core::interaction::{response_approved, response_as_choice, response_as_text};

// ─── Shared taint-gate prompt helpers ──────────────────────────────────────
// Used by both the worker (IPC) and foreground (stdin) GatePrompt impls so the
// decision-parsing / arg-building / approval-mapping logic is written and
// tested once, not duplicated across two untestable I/O closures.

/// Extract `(tool_name, taint, clearance)` from a blocked gate decision, or
/// `None` if the decision isn't a block.
pub fn gate_block_info(
    decision: &leviath_core::taint::GateDecision,
) -> Option<(String, leviath_core::TaintLevel, leviath_core::TaintLevel)> {
    match decision {
        leviath_core::taint::GateDecision::Blocked {
            tool_name,
            taint_level,
            clearance,
            ..
        } => Some((tool_name.clone(), *taint_level, *clearance)),
        _ => None,
    }
}

/// Build the approval-prompt arguments explaining why an outbound call was gated.
pub fn gate_prompt_args(
    tool_name: &str,
    taint: leviath_core::TaintLevel,
    clearance: leviath_core::TaintLevel,
) -> serde_json::Value {
    serde_json::json!({
        "taint_gate": true,
        "reason": format!(
            "Outbound tool '{}' would carry {}-sensitivity data above its {} clearance.",
            tool_name, taint, clearance
        ),
    })
}

/// Map an approval outcome (approved, session-scope) to a gate resolution.
pub fn map_gate_approval(approved: bool, session: bool) -> crate::taint::GateResolution {
    use crate::taint::GateResolution;
    match (approved, session) {
        (false, _) => GateResolution::Deny,
        (true, true) => GateResolution::AlwaysAllow,
        (true, false) => GateResolution::AllowOnce,
    }
}

/// Resolve a foreground taint-gate block by asking via `ask` (real stdin in
/// production, a mock in tests) and mapping the response. Kept free of the
/// blocking stdin call itself so the request-building + mapping are testable.
pub fn resolve_gate_with_asker(
    decision: &leviath_core::taint::GateDecision,
    stage_name: &str,
    ask: impl Fn(&InteractionRequest) -> InteractionResponse,
) -> crate::taint::GateResolution {
    use crate::taint::GateResolution;
    let Some((tool_name, taint, clearance)) = gate_block_info(decision) else {
        return GateResolution::AllowOnce;
    };
    let req = InteractionRequest::tool_approval(
        format!("taint-{}", tool_name),
        &tool_name,
        gate_prompt_args(&tool_name, taint, clearance),
        stage_name,
    );
    let resp = ask(&req);
    map_gate_approval(
        response_approved(&resp),
        resp.scope == Some(ApprovalScope::Session),
    )
}

/// How a dynamically-requested interaction is dispatched and logged.
///
/// The background worker answers via the file-based IPC channel and logs to
/// the per-stage log file; the foreground path answers via stdin and prints
/// directly. Both share the exact same tool-argument parsing and response
/// formatting in [`dispatch_dynamic_interaction`].
#[async_trait]
pub trait InteractionBackend: Send + Sync {
    /// Block until the user answers `req`.
    async fn ask(&self, req: InteractionRequest) -> InteractionResponse;

    /// Record an operational log line. No-op by default (the foreground
    /// path has no per-stage log file to write to).
    fn log(&self, message: &str) {
        let _ = message;
    }

    /// Called only for `present_for_review`, once, before asking: persist
    /// or display the document. No-op by default.
    fn on_review_document(&self, tool_call_id: &str, title: &str, markdown: &str) {
        let _ = (tool_call_id, title, markdown);
    }
}

/// What an unattended run tells the model when a question needed a person.
pub const UNATTENDED_NO_ANSWER: &str =
    "[unattended run] No user was available to answer (--yolo). Decide for yourself and continue.";

/// The answer an unattended run (`--yolo`) gives a request nobody is there to
/// see.
///
/// `--yolo` means "run without a human", so a prompt that blocks on one would
/// park the run forever - a headless run would hang at the first
/// `ask_user_confirm`.
///
/// A confirmation is approved: that is exactly what the flag promises. A
/// *choice* is deliberately **not** made - picking option 0 unseen could select
/// "Abort" or a destructive branch - so the model is told no one answered and
/// left to decide. An edit submits the document unchanged, and a document put up
/// for review is acknowledged without comment (a review is a `FreeText` request
/// carrying a `body`; a question is one without).
pub fn unattended_answer(req: &InteractionRequest) -> InteractionResponse {
    use leviath_core::interaction::InteractionKind;
    match req.kind {
        InteractionKind::Confirm | InteractionKind::ToolApproval => {
            InteractionResponse::approval(&req.id, true, ApprovalScope::Once)
        }
        InteractionKind::EditText => {
            InteractionResponse::text(&req.id, req.body.clone().unwrap_or_default())
        }
        InteractionKind::FreeText if req.body.is_some() => InteractionResponse::text(&req.id, ""),
        InteractionKind::FreeText | InteractionKind::MultipleChoice => {
            InteractionResponse::text(&req.id, UNATTENDED_NO_ANSWER)
        }
    }
}

/// An [`InteractionBackend`] for unattended runs: answers every request from
/// [`unattended_answer`] instead of opening a prompt on the hub.
pub struct UnattendedInteraction;

#[async_trait]
impl InteractionBackend for UnattendedInteraction {
    async fn ask(&self, req: InteractionRequest) -> InteractionResponse {
        unattended_answer(&req)
    }
}

/// The tools that suspend the agent until a person answers.
///
/// Every name here is handled by [`dispatch_dynamic_interaction`] below, which
/// hands the call to the interaction backend and awaits a human response - so a
/// stage that offers one of these with nobody attached parks there for as long
/// as the run lives. `all_dynamic_interaction_tool_names_are_handled` iterates
/// this list, so the two cannot drift.
///
/// Blueprint linting reads it to flag an autonomous stage that grants one.
pub const BLOCKING_INTERACTION_TOOLS: &[&str] = &[
    "present_for_review",
    "ask_user_text",
    "ask_user_choice",
    "ask_user_confirm",
    "edit_document",
];

/// Dispatch a single dynamic-interaction tool call.
///
/// Returns `Some(result_string)` if `tool_name` is one of
/// `present_for_review` / `ask_user_text` / `ask_user_choice` /
/// `ask_user_confirm` (and was therefore handled here); returns `None` for
/// any other tool name so the caller can fall through to normal tool dispatch.
pub async fn dispatch_dynamic_interaction(
    backend: &dyn InteractionBackend,
    tool_name: &str,
    tool_call_id: &str,
    arguments: &serde_json::Value,
    stage_name: &str,
) -> Option<String> {
    match tool_name {
        "present_for_review" => {
            Some(handle_present_for_review(backend, tool_call_id, arguments, stage_name).await)
        }
        "ask_user_text" => {
            Some(handle_ask_user_text(backend, tool_call_id, arguments, stage_name).await)
        }
        "ask_user_choice" => {
            Some(handle_ask_user_choice(backend, tool_call_id, arguments, stage_name).await)
        }
        "ask_user_confirm" => {
            Some(handle_ask_user_confirm(backend, tool_call_id, arguments, stage_name).await)
        }
        "edit_document" => {
            Some(handle_edit_document(backend, tool_call_id, arguments, stage_name).await)
        }
        _ => None,
    }
}

fn arg_str<'a>(arguments: &'a serde_json::Value, key: &str, default: &'a str) -> String {
    arguments
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or(default)
        .to_string()
}

async fn handle_present_for_review(
    backend: &dyn InteractionBackend,
    tool_call_id: &str,
    arguments: &serde_json::Value,
    stage_name: &str,
) -> String {
    let title = arg_str(arguments, "title", "Review");
    let markdown = arg_str(arguments, "markdown", "");

    backend.on_review_document(tool_call_id, &title, &markdown);
    backend.log(&format!(
        "[tool] present_for_review \u{2192} waiting for user review: {}",
        title
    ));

    let req = InteractionRequest::review(
        format!("review-{}", tool_call_id),
        &title,
        &markdown,
        stage_name,
    );
    let resp = backend.ask(req).await;
    let user_feedback = response_as_text(&resp);

    backend.log("[tool] present_for_review \u{2192} done");

    if user_feedback.trim().is_empty() {
        "User reviewed the document and acknowledged.".to_string()
    } else {
        format!("User feedback: {}", user_feedback)
    }
}

async fn handle_ask_user_text(
    backend: &dyn InteractionBackend,
    tool_call_id: &str,
    arguments: &serde_json::Value,
    stage_name: &str,
) -> String {
    let prompt = arg_str(arguments, "prompt", "");

    backend.log(&format!(
        "[tool] ask_user_text \u{2192} waiting: {}",
        prompt
    ));

    let req =
        InteractionRequest::free_text(format!("ask-{}", tool_call_id), &prompt, stage_name, true);
    let resp = backend.ask(req).await;
    let answer = response_as_text(&resp);

    backend.log("[tool] ask_user_text \u{2192} done");

    if answer.trim().is_empty() {
        "User provided no answer.".to_string()
    } else {
        answer
    }
}

async fn handle_ask_user_choice(
    backend: &dyn InteractionBackend,
    tool_call_id: &str,
    arguments: &serde_json::Value,
    stage_name: &str,
) -> String {
    let prompt = arg_str(arguments, "prompt", "");
    let options: Vec<String> = arguments
        .get("options")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    if options.len() < 2 {
        return "[error] ask_user_choice requires at least 2 options".to_string();
    }

    backend.log(&format!(
        "[tool] ask_user_choice \u{2192} waiting: {}",
        prompt
    ));

    let req = InteractionRequest::multiple_choice(
        format!("ask-{}", tool_call_id),
        &prompt,
        options.clone(),
        stage_name,
    );
    let resp = backend.ask(req).await;
    let choice = response_as_choice(&resp, &options)
        .cloned()
        .unwrap_or_else(|| response_as_text(&resp));

    backend.log("[tool] ask_user_choice \u{2192} done");

    format!("User chose: {}", choice)
}

async fn handle_ask_user_confirm(
    backend: &dyn InteractionBackend,
    tool_call_id: &str,
    arguments: &serde_json::Value,
    stage_name: &str,
) -> String {
    let prompt = arg_str(arguments, "prompt", "");

    backend.log(&format!(
        "[tool] ask_user_confirm \u{2192} waiting: {}",
        prompt
    ));

    let req = InteractionRequest::confirm(format!("ask-{}", tool_call_id), &prompt, stage_name);
    let resp = backend.ask(req).await;
    let approved = response_approved(&resp);

    backend.log("[tool] ask_user_confirm \u{2192} done");

    format!("User answered: {}", if approved { "Yes" } else { "No" })
}

async fn handle_edit_document(
    backend: &dyn InteractionBackend,
    tool_call_id: &str,
    arguments: &serde_json::Value,
    stage_name: &str,
) -> String {
    let content = arg_str(arguments, "content", "");
    let prompt = arg_str(
        arguments,
        "prompt",
        "Edit the document below, then submit your changes:",
    );

    backend.log("[tool] edit_document \u{2192} waiting for user edits");

    let req = InteractionRequest::edit_text(
        format!("edit-{}", tool_call_id),
        &prompt,
        stage_name,
        &content,
    );
    let resp = backend.ask(req).await;
    let edited = response_as_text(&resp);

    backend.log("[tool] edit_document \u{2192} done");

    if edited.trim().is_empty() {
        format!("User made no changes. Current document:\n{}", content)
    } else {
        format!("User-edited document:\n{}", edited)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records every `ask()` request and `log()`/`on_review_document()` call,
    /// and returns a pre-scripted response for each `ask()` in order.
    #[derive(Default)]
    struct MockBackend {
        responses: Mutex<Vec<InteractionResponse>>,
        asked: Mutex<Vec<InteractionRequest>>,
        logs: Mutex<Vec<String>>,
        reviews: Mutex<Vec<(String, String, String)>>,
    }

    impl MockBackend {
        fn with_responses(responses: Vec<InteractionResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
                ..Default::default()
            }
        }
    }

    #[async_trait]
    impl InteractionBackend for MockBackend {
        async fn ask(&self, req: InteractionRequest) -> InteractionResponse {
            self.asked.lock().unwrap().push(req);
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                InteractionResponse::text("", "")
            } else {
                responses.remove(0)
            }
        }

        fn log(&self, message: &str) {
            self.logs.lock().unwrap().push(message.to_string());
        }

        fn on_review_document(&self, tool_call_id: &str, title: &str, markdown: &str) {
            self.reviews.lock().unwrap().push((
                tool_call_id.to_string(),
                title.to_string(),
                markdown.to_string(),
            ));
        }
    }

    // ─── dispatch_dynamic_interaction: routing ─────────────────────────────

    #[tokio::test]
    async fn dispatch_unknown_tool_returns_none() {
        let backend = MockBackend::default();
        let result = dispatch_dynamic_interaction(
            &backend,
            "read_file",
            "id1",
            &serde_json::json!({}),
            "main",
        )
        .await;
        assert!(result.is_none());
        assert!(backend.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn all_dynamic_interaction_tool_names_are_handled() {
        for name in BLOCKING_INTERACTION_TOOLS.iter().copied() {
            let backend = MockBackend::with_responses(vec![InteractionResponse::text("", "ok")]);
            let result = dispatch_dynamic_interaction(
                &backend,
                name,
                "id1",
                &serde_json::json!({"title": "t", "markdown": "m", "prompt": "p", "options": ["A", "B"]}),
                "main",
            )
            .await;
            assert!(result.is_some());
        }
    }

    // ─── present_for_review ─────────────────────────────────────────────────

    #[tokio::test]
    async fn present_for_review_persists_document_before_asking() {
        let backend = MockBackend::with_responses(vec![InteractionResponse::text("", "")]);
        let result = dispatch_dynamic_interaction(
            &backend,
            "present_for_review",
            "call1",
            &serde_json::json!({"title": "My Plan", "markdown": "# Plan\ndetails"}),
            "plan",
        )
        .await
        .unwrap();

        assert_eq!(result, "User reviewed the document and acknowledged.");
        let reviews = backend.reviews.lock().unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].0, "call1");
        assert_eq!(reviews[0].1, "My Plan");
        assert_eq!(reviews[0].2, "# Plan\ndetails");
    }

    #[tokio::test]
    async fn present_for_review_returns_feedback_when_given() {
        let backend =
            MockBackend::with_responses(vec![InteractionResponse::text("", "looks great")]);
        let result = dispatch_dynamic_interaction(
            &backend,
            "present_for_review",
            "call2",
            &serde_json::json!({"title": "Design", "markdown": "body"}),
            "plan",
        )
        .await
        .unwrap();
        assert_eq!(result, "User feedback: looks great");
    }

    #[tokio::test]
    async fn present_for_review_defaults_missing_title_and_markdown() {
        let backend = MockBackend::with_responses(vec![InteractionResponse::text("", "")]);
        dispatch_dynamic_interaction(
            &backend,
            "present_for_review",
            "call3",
            &serde_json::json!({}),
            "plan",
        )
        .await;
        let reviews = backend.reviews.lock().unwrap();
        assert_eq!(reviews[0].1, "Review");
        assert_eq!(reviews[0].2, "");
    }

    #[tokio::test]
    async fn present_for_review_builds_review_kind_request() {
        let backend = MockBackend::with_responses(vec![InteractionResponse::text("", "")]);
        dispatch_dynamic_interaction(
            &backend,
            "present_for_review",
            "call4",
            &serde_json::json!({"title": "T", "markdown": "M"}),
            "plan",
        )
        .await;
        let asked = backend.asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].id, "review-call4");
        assert_eq!(asked[0].prompt, "T");
        assert_eq!(asked[0].body.as_deref(), Some("M"));
        assert_eq!(
            asked[0].body_format,
            leviath_core::interaction::BodyFormat::Markdown
        );
        assert_eq!(asked[0].stage_name, "plan");
    }

    #[tokio::test]
    async fn present_for_review_logs_waiting_and_done() {
        let backend = MockBackend::with_responses(vec![InteractionResponse::text("", "")]);
        dispatch_dynamic_interaction(
            &backend,
            "present_for_review",
            "call5",
            &serde_json::json!({"title": "T", "markdown": "M"}),
            "plan",
        )
        .await;
        let logs = backend.logs.lock().unwrap();
        assert!(logs[0].contains("waiting for user review: T"));
        assert!(logs[1].contains("done"));
    }

    // ─── ask_user_text ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn ask_user_text_returns_answer() {
        let backend = MockBackend::with_responses(vec![InteractionResponse::text("", "blue")]);
        let result = dispatch_dynamic_interaction(
            &backend,
            "ask_user_text",
            "call1",
            &serde_json::json!({"prompt": "What color?"}),
            "plan",
        )
        .await
        .unwrap();
        assert_eq!(result, "blue");
    }

    #[tokio::test]
    async fn ask_user_text_empty_answer_reports_no_answer() {
        let backend = MockBackend::with_responses(vec![InteractionResponse::text("", "  ")]);
        let result = dispatch_dynamic_interaction(
            &backend,
            "ask_user_text",
            "call2",
            &serde_json::json!({"prompt": "Anything?"}),
            "plan",
        )
        .await
        .unwrap();
        assert_eq!(result, "User provided no answer.");
    }

    #[tokio::test]
    async fn ask_user_text_builds_free_text_required_request() {
        let backend = MockBackend::with_responses(vec![InteractionResponse::text("", "x")]);
        dispatch_dynamic_interaction(
            &backend,
            "ask_user_text",
            "call3",
            &serde_json::json!({"prompt": "Q?"}),
            "implement",
        )
        .await;
        let asked = backend.asked.lock().unwrap();
        assert_eq!(asked[0].id, "ask-call3");
        assert_eq!(asked[0].prompt, "Q?");
        assert!(asked[0].required);
        assert_eq!(
            asked[0].kind,
            leviath_core::interaction::InteractionKind::FreeText
        );
        assert_eq!(asked[0].stage_name, "implement");
    }

    #[tokio::test]
    async fn ask_user_text_missing_prompt_defaults_empty() {
        let backend = MockBackend::with_responses(vec![InteractionResponse::text("", "x")]);
        dispatch_dynamic_interaction(
            &backend,
            "ask_user_text",
            "call4",
            &serde_json::json!({}),
            "plan",
        )
        .await;
        let asked = backend.asked.lock().unwrap();
        assert_eq!(asked[0].prompt, "");
    }

    // ─── ask_user_choice ────────────────────────────────────────────────────

    #[tokio::test]
    async fn ask_user_choice_returns_chosen_option() {
        let backend = MockBackend::with_responses(vec![InteractionResponse::choice("", 1)]);
        let result = dispatch_dynamic_interaction(
            &backend,
            "ask_user_choice",
            "call1",
            &serde_json::json!({"prompt": "Pick one", "options": ["A", "B", "C"]}),
            "plan",
        )
        .await
        .unwrap();
        assert_eq!(result, "User chose: B");
    }

    #[tokio::test]
    async fn ask_user_choice_falls_back_to_text_when_no_choice_index() {
        let backend =
            MockBackend::with_responses(vec![InteractionResponse::text("", "custom answer")]);
        let result = dispatch_dynamic_interaction(
            &backend,
            "ask_user_choice",
            "call2",
            &serde_json::json!({"prompt": "Pick one", "options": ["A", "B"]}),
            "plan",
        )
        .await
        .unwrap();
        assert_eq!(result, "User chose: custom answer");
    }

    #[tokio::test]
    async fn ask_user_choice_rejects_fewer_than_two_options() {
        let backend = MockBackend::default();
        let result = dispatch_dynamic_interaction(
            &backend,
            "ask_user_choice",
            "call3",
            &serde_json::json!({"prompt": "Pick one", "options": ["A"]}),
            "plan",
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            "[error] ask_user_choice requires at least 2 options"
        );
        // Must not have asked the user anything for an invalid call.
        assert!(backend.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ask_user_choice_rejects_missing_options() {
        let backend = MockBackend::default();
        let result = dispatch_dynamic_interaction(
            &backend,
            "ask_user_choice",
            "call4",
            &serde_json::json!({"prompt": "Pick one"}),
            "plan",
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            "[error] ask_user_choice requires at least 2 options"
        );
    }

    #[tokio::test]
    async fn ask_user_choice_builds_multiple_choice_request() {
        let backend = MockBackend::with_responses(vec![InteractionResponse::choice("", 0)]);
        dispatch_dynamic_interaction(
            &backend,
            "ask_user_choice",
            "call5",
            &serde_json::json!({"prompt": "Q?", "options": ["X", "Y"]}),
            "plan",
        )
        .await;
        let asked = backend.asked.lock().unwrap();
        assert_eq!(asked[0].id, "ask-call5");
        assert_eq!(
            asked[0].kind,
            leviath_core::interaction::InteractionKind::MultipleChoice
        );
        assert_eq!(asked[0].options, vec!["X".to_string(), "Y".to_string()]);
    }

    // ─── ask_user_confirm ───────────────────────────────────────────────────

    #[tokio::test]
    async fn ask_user_confirm_yes() {
        let backend = MockBackend::with_responses(vec![InteractionResponse::approval(
            "",
            true,
            leviath_core::interaction::ApprovalScope::Once,
        )]);
        let result = dispatch_dynamic_interaction(
            &backend,
            "ask_user_confirm",
            "call1",
            &serde_json::json!({"prompt": "Proceed?"}),
            "implement",
        )
        .await
        .unwrap();
        assert_eq!(result, "User answered: Yes");
    }

    #[tokio::test]
    async fn ask_user_confirm_no() {
        let backend = MockBackend::with_responses(vec![InteractionResponse::approval(
            "",
            false,
            leviath_core::interaction::ApprovalScope::Once,
        )]);
        let result = dispatch_dynamic_interaction(
            &backend,
            "ask_user_confirm",
            "call2",
            &serde_json::json!({"prompt": "Proceed?"}),
            "implement",
        )
        .await
        .unwrap();
        assert_eq!(result, "User answered: No");
    }

    #[tokio::test]
    async fn ask_user_confirm_defaults_to_no_when_unanswered() {
        // response_approved() defaults false for a response with no `approved` set.
        let backend = MockBackend::with_responses(vec![InteractionResponse::text("", "")]);
        let result = dispatch_dynamic_interaction(
            &backend,
            "ask_user_confirm",
            "call3",
            &serde_json::json!({"prompt": "Proceed?"}),
            "implement",
        )
        .await
        .unwrap();
        assert_eq!(result, "User answered: No");
    }

    #[tokio::test]
    async fn ask_user_confirm_builds_confirm_request() {
        let backend = MockBackend::with_responses(vec![InteractionResponse::approval(
            "",
            true,
            leviath_core::interaction::ApprovalScope::Once,
        )]);
        dispatch_dynamic_interaction(
            &backend,
            "ask_user_confirm",
            "call4",
            &serde_json::json!({"prompt": "Sure?"}),
            "implement",
        )
        .await;
        let asked = backend.asked.lock().unwrap();
        assert_eq!(asked[0].id, "ask-call4");
        assert_eq!(
            asked[0].kind,
            leviath_core::interaction::InteractionKind::Confirm
        );
        assert_eq!(asked[0].options, vec!["Yes".to_string(), "No".to_string()]);
    }

    // ─── edit_document ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn edit_document_returns_edited_text() {
        let backend =
            MockBackend::with_responses(vec![InteractionResponse::text("", "edited plan")]);
        let result = dispatch_dynamic_interaction(
            &backend,
            "edit_document",
            "call1",
            &serde_json::json!({"content": "original plan"}),
            "plan",
        )
        .await
        .unwrap();

        assert_eq!(result, "User-edited document:\nedited plan");
        let asked = backend.asked.lock().unwrap();
        assert_eq!(asked[0].id, "edit-call1");
        assert_eq!(
            asked[0].kind,
            leviath_core::interaction::InteractionKind::EditText
        );
        assert_eq!(asked[0].body.as_deref(), Some("original plan"));
    }

    #[tokio::test]
    async fn edit_document_empty_edit_returns_original_content() {
        let backend = MockBackend::with_responses(vec![InteractionResponse::text("", "")]);
        let result = dispatch_dynamic_interaction(
            &backend,
            "edit_document",
            "call2",
            &serde_json::json!({"content": "keep this"}),
            "plan",
        )
        .await
        .unwrap();
        assert_eq!(result, "User made no changes. Current document:\nkeep this");
    }

    // ─── log() / on_review_document() default no-ops don't panic ──────────

    struct NoopBackend;

    #[async_trait]
    impl InteractionBackend for NoopBackend {
        async fn ask(&self, _req: InteractionRequest) -> InteractionResponse {
            InteractionResponse::text("", "answer")
        }
    }

    #[tokio::test]
    async fn default_log_and_review_hooks_are_noop_and_safe() {
        let backend = NoopBackend;
        let result = dispatch_dynamic_interaction(
            &backend,
            "ask_user_text",
            "call1",
            &serde_json::json!({"prompt": "Q?"}),
            "plan",
        )
        .await;
        assert_eq!(result, Some("answer".to_string()));

        let result = dispatch_dynamic_interaction(
            &backend,
            "present_for_review",
            "call2",
            &serde_json::json!({"title": "T", "markdown": "M"}),
            "plan",
        )
        .await;
        assert_eq!(result, Some("User feedback: answer".to_string()));
    }

    // ─── taint-gate prompt helpers ──────────────────────────────────────────

    fn blocked_decision(tool: &str) -> leviath_core::taint::GateDecision {
        leviath_core::taint::GateDecision::Blocked {
            taint_level: leviath_core::TaintLevel::Private,
            clearance: leviath_core::TaintLevel::Public,
            source_regions: vec!["notes".into()],
            tool_name: tool.to_string(),
        }
    }

    #[test]
    fn gate_block_info_extracts_blocked_fields() {
        let (tool, taint, clearance) = gate_block_info(&blocked_decision("shell")).unwrap();
        assert_eq!(tool, "shell");
        assert_eq!(taint, leviath_core::TaintLevel::Private);
        assert_eq!(clearance, leviath_core::TaintLevel::Public);
        // Allowed decisions yield None.
        assert!(gate_block_info(&leviath_core::taint::GateDecision::Allowed).is_none());
    }

    #[test]
    fn gate_prompt_args_mentions_tool() {
        let args = gate_prompt_args(
            "send_email",
            leviath_core::TaintLevel::Private,
            leviath_core::TaintLevel::Public,
        );
        assert_eq!(args["taint_gate"], true);
        assert!(args["reason"].as_str().unwrap().contains("send_email"));
    }

    #[test]
    fn map_gate_approval_covers_all_outcomes() {
        use crate::taint::GateResolution;
        assert_eq!(map_gate_approval(false, false), GateResolution::Deny);
        assert_eq!(map_gate_approval(false, true), GateResolution::Deny);
        assert_eq!(map_gate_approval(true, false), GateResolution::AllowOnce);
        assert_eq!(map_gate_approval(true, true), GateResolution::AlwaysAllow);
    }

    #[test]
    fn resolve_gate_with_asker_maps_response() {
        use crate::taint::GateResolution;
        // Deny.
        let r = resolve_gate_with_asker(&blocked_decision("shell"), "plan", |_req| {
            InteractionResponse::approval("", false, ApprovalScope::Once)
        });
        assert_eq!(r, GateResolution::Deny);
        // Always-allow (session scope). Also assert the request the asker saw is
        // a taint-gate tool-approval for the right tool.
        let r = resolve_gate_with_asker(&blocked_decision("shell"), "plan", |req| {
            assert_eq!(req.tool_name.as_deref(), Some("shell"));
            assert_eq!(req.stage_name, "plan");
            InteractionResponse::approval("", true, ApprovalScope::Session)
        });
        assert_eq!(r, GateResolution::AlwaysAllow);
        // A text response (no approval) denies. Bind the asker as a fn pointer
        // (Copy) so its body is exercised here, then reuse it below where the
        // short-circuit means it is never invoked.
        let text_asker: fn(&InteractionRequest) -> InteractionResponse =
            |_req| InteractionResponse::text("", "");
        let denied = resolve_gate_with_asker(&blocked_decision("shell"), "plan", text_asker);
        assert_eq!(denied, GateResolution::Deny);
        // A non-block decision short-circuits to AllowOnce without asking - the
        // (already-covered) asker is never invoked.
        let r = resolve_gate_with_asker(
            &leviath_core::taint::GateDecision::Allowed,
            "plan",
            text_asker,
        );
        assert_eq!(r, GateResolution::AllowOnce);
    }

    // ── unattended (--yolo) answers ───────────────────────────────────────

    #[tokio::test]
    async fn unattended_answers_every_prompt_without_a_hub() {
        // Issue #107: `--yolo` means "run without a human", so a prompt that
        // waits for one parks the run forever. Every dynamic-interaction tool
        // must come back with something the model can act on.
        let backend = UnattendedInteraction;

        // A confirmation is approved - that is what the flag promises.
        let confirmed = dispatch_dynamic_interaction(
            &backend,
            "ask_user_confirm",
            "c1",
            &serde_json::json!({"prompt": "Delete the branch?"}),
            "implement",
        )
        .await
        .unwrap();
        assert_eq!(confirmed, "User answered: Yes");

        // A *choice* is deliberately left unmade: picking an option unseen could
        // select "Abort" or a destructive branch, so the model is told nobody
        // answered and decides for itself.
        let chosen = dispatch_dynamic_interaction(
            &backend,
            "ask_user_choice",
            "c2",
            &serde_json::json!({"prompt": "Which?", "options": ["Ship it", "Abort"]}),
            "implement",
        )
        .await
        .unwrap();
        assert!(chosen.contains(UNATTENDED_NO_ANSWER), "got: {chosen}");
        assert!(!chosen.contains("Abort"), "no option may be picked blind");

        // Free text says so plainly.
        let answered = dispatch_dynamic_interaction(
            &backend,
            "ask_user_text",
            "c3",
            &serde_json::json!({"prompt": "Which database?"}),
            "implement",
        )
        .await
        .unwrap();
        assert_eq!(answered, UNATTENDED_NO_ANSWER);

        // A review is acknowledged, and an edit submits the document unchanged.
        let reviewed = dispatch_dynamic_interaction(
            &backend,
            "present_for_review",
            "c4",
            &serde_json::json!({"title": "Plan", "markdown": "# Plan"}),
            "plan",
        )
        .await
        .unwrap();
        assert!(reviewed.contains("acknowledged"), "got: {reviewed}");

        let edited = dispatch_dynamic_interaction(
            &backend,
            "edit_document",
            "c5",
            &serde_json::json!({"content": "keep me"}),
            "plan",
        )
        .await
        .unwrap();
        assert!(edited.contains("keep me"), "got: {edited}");
    }

    #[test]
    fn unattended_answer_approves_a_tool_approval() {
        // The tool-policy layer normally short-circuits these under --yolo, so
        // cover the arm directly.
        let req = InteractionRequest::tool_approval("t1", "shell", serde_json::json!({}), "impl");
        let resp = unattended_answer(&req);
        assert!(leviath_core::interaction::response_approved(&resp));
        assert_eq!(resp.scope, Some(ApprovalScope::Once));
    }
}
