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
