//! Agent-initiated dynamic interaction tools: `present_for_review`,
//! `ask_user_text`, `ask_user_choice`, `ask_user_confirm`.
//!
//! Unlike `interaction_points` (declared statically in a blueprint and
//! always fired), these are ordinary tool calls the model makes on its own
//! judgment, mid-reasoning. Both the background worker (file-based IPC) and
//! the foreground (stdin) run modes need to intercept these tool names
//! before they ever reach the generic tool registry — this module holds
//! that shared logic behind an [`InteractionBackend`] trait so it can be
//! unit tested with a mock instead of only living inside untestable
//! closures.

use async_trait::async_trait;

use crate::interaction::{response_approved, response_as_choice, response_as_text};
use crate::interaction::{InteractionRequest, InteractionResponse};

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
        format!("User: {}", answer)
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
        let names = [
            "present_for_review",
            "ask_user_text",
            "ask_user_choice",
            "ask_user_confirm",
        ];
        for name in names {
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
            crate::interaction::BodyFormat::Markdown
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
        assert_eq!(result, "User: blue");
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
        assert_eq!(asked[0].kind, crate::interaction::InteractionKind::FreeText);
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
            crate::interaction::InteractionKind::MultipleChoice
        );
        assert_eq!(asked[0].options, vec!["X".to_string(), "Y".to_string()]);
    }

    // ─── ask_user_confirm ───────────────────────────────────────────────────

    #[tokio::test]
    async fn ask_user_confirm_yes() {
        let backend = MockBackend::with_responses(vec![InteractionResponse::approval(
            "",
            true,
            crate::interaction::ApprovalScope::Once,
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
            crate::interaction::ApprovalScope::Once,
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
            crate::interaction::ApprovalScope::Once,
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
        assert_eq!(asked[0].kind, crate::interaction::InteractionKind::Confirm);
        assert_eq!(asked[0].options, vec!["Yes".to_string(), "No".to_string()]);
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
        assert_eq!(result, Some("User: answer".to_string()));

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
}
