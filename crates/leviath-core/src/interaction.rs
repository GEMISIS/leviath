//! Plain value types for the worker ↔ dashboard interaction channel.
//!
//! These are serde data types shared across the engine (`leviath-runtime`),
//! the CLI's file-IPC/stdin transports, and the dashboard. The concrete
//! transport functions (file IPC, stdin) and backends live in `leviath-cli`;
//! only the wire/value types and their pure resolver helpers live here so the
//! runtime can reference them without depending on the CLI.

use serde::{Deserialize, Serialize};

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
    /// Edit a document in place: the request carries the current text in
    /// `body`; the user edits it and the (possibly modified) text is returned.
    EditText,
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

    /// Create an "edit document" request: shows `initial_content` in an
    /// editable field pre-seeded with it (via `body`); the user edits it and
    /// the modified text is returned as the response text.
    pub fn edit_text(
        id: impl Into<String>,
        prompt: impl Into<String>,
        stage: impl Into<String>,
        initial_content: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind: InteractionKind::EditText,
            prompt: prompt.into(),
            options: vec![],
            tool_name: None,
            tool_arguments: None,
            required: true,
            stage_name: stage.into(),
            body: Some(initial_content.into()),
            body_format: BodyFormat::Plain,
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

// ─── Pure resolver helpers ────────────────────────────────────────────────────

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

#[cfg(test)]
mod tests {
    use super::*;

    // ─── InteractionRequest / InteractionResponse constructors ─────────────

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

    #[test]
    fn test_interaction_response_serde_roundtrip() {
        let resp = InteractionResponse::approval("serde2", true, ApprovalScope::Session);
        let json = serde_json::to_string(&resp).unwrap();
        let back: InteractionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.request_id, "serde2");
        assert_eq!(back.approved, Some(true));
        assert_eq!(back.scope, Some(ApprovalScope::Session));
    }

    #[test]
    fn test_make_interaction_id_zero() {
        assert_eq!(make_interaction_id(0, 0), "0-0");
    }

    #[test]
    fn test_make_interaction_id_large() {
        assert_eq!(make_interaction_id(999, 1000), "999-1000");
    }

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

    #[test]
    fn test_response_as_text_with_value() {
        let r = InteractionResponse::text("id", "some text value");
        assert_eq!(response_as_text(&r), "some text value");
    }

    #[test]
    fn test_response_approved_session_scope() {
        let r = InteractionResponse::approval("id", true, ApprovalScope::Session);
        assert!(response_approved(&r));
        assert_eq!(r.scope, Some(ApprovalScope::Session));
    }

    #[test]
    fn test_make_interaction_id_various() {
        assert_eq!(make_interaction_id(1, 2), "1-2");
        assert_eq!(make_interaction_id(10, 20), "10-20");
    }

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
}
