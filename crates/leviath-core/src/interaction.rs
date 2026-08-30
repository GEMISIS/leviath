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
    /// Markdown body - rendered via the dashboard's markdown renderer.
    Markdown,
}

/// A pending interaction request written by the worker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    #[serde(default = "crate::default_true")]
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
    /// The part of a tool call a person needs to see before approving it.
    ///
    /// "Allow tool call: `bash`?" is not a question anyone can answer - it asks
    /// whether to run *a shell command* without saying which one, so the only
    /// safe answer is no and the only practical one is yes. The argument that
    /// decides the answer is the command itself, and for the file tools it is
    /// the path.
    ///
    /// Truncated, because a prompt is a line in a terminal: a heredoc that
    /// scrolls the decision off screen is the same problem again.
    fn approval_detail(tool: &str, arguments: &serde_json::Value) -> Option<String> {
        let field = match tool {
            "bash" | "shell" => "command",
            "write_file" | "edit_file" | "read_file" => "path",
            _ => return None,
        };
        let raw = arguments.get(field)?.as_str()?.trim();
        if raw.is_empty() {
            return None;
        }
        // One line: a multi-line command is summarised by its first line so the
        // prompt stays readable, with the rest available in `tool_arguments`.
        let first = raw.lines().next().unwrap_or(raw);
        let mut shown: String = first.chars().take(120).collect();
        if shown.chars().count() < first.chars().count() || first.len() < raw.len() {
            shown.push('…');
        }
        Some(format!("`{shown}`"))
    }

    /// Build a taint-gate approval request.
    ///
    /// Shaped like a tool approval - it is the same yes/no with a scope - but
    /// worded for the decision actually being made. A gate approval clears the
    /// tool to carry the data, which is not a grant keyed on what the call
    /// runs, so it offers no per-stage scope and names no keys.
    pub fn gate_approval(
        id: impl Into<String>,
        tool_name: impl Into<String>,
        arguments: serde_json::Value,
        stage: impl Into<String>,
    ) -> Self {
        let tool = tool_name.into();
        Self {
            id: id.into(),
            kind: InteractionKind::ToolApproval,
            prompt: format!("Allow tool call: `{tool}`?"),
            options: vec![
                "Allow once".to_string(),
                "Allow for this run".to_string(),
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

    /// What a scoped approval of this call would grant, worded for an option
    /// label. `None` when the call has no reusable key, which is what makes the
    /// "it will ask again" wording honest rather than a surprise.
    ///
    /// Three keys then `+N more`: the point is to show what is being handed
    /// over, and a label that wraps the terminal shows nothing.
    fn grant_summary(grant_keys: &[String]) -> Option<String> {
        if grant_keys.is_empty() {
            return None;
        }
        let named: Vec<&str> = grant_keys
            .iter()
            .take(3)
            .map(|k| k.strip_prefix("shell:").unwrap_or(k))
            .collect();
        let mut summary = named.join(", ");
        if grant_keys.len() > named.len() {
            summary.push_str(&format!(" +{} more", grant_keys.len() - named.len()));
        }
        Some(summary)
    }

    /// Build a tool-approval request.
    ///
    /// `grant_keys` is what a `Stage` or `Run` approval would be remembered
    /// under, and it goes in the option labels rather than in `body` because
    /// the dashboard renders only `prompt` and `options` for this kind. Naming
    /// it matters: a label promising more than the dispatcher will remember -
    /// "Allow for this session" on a call it degrades to once - has the user
    /// choose a grant they do not get.
    ///
    /// The options are fixed-position - every client maps an index to a scope
    /// through [`approval_choice`] - so the wording varies and the length does
    /// not.
    pub fn tool_approval(
        id: impl Into<String>,
        tool_name: impl Into<String>,
        arguments: serde_json::Value,
        stage: impl Into<String>,
        grant_keys: &[String],
    ) -> Self {
        let tool = tool_name.into();
        let prompt = match Self::approval_detail(&tool, &arguments) {
            Some(detail) => format!("Allow tool call: `{tool}` - {detail}?"),
            None => format!("Allow tool call: `{tool}`?"),
        };
        let (stage_label, run_label) = match Self::grant_summary(grant_keys) {
            Some(what) => (
                format!("Allow {what} for this stage"),
                format!("Allow {what} for this run"),
            ),
            None => (
                "Allow for this stage (nothing reusable - it will ask again)".to_string(),
                "Allow for this run (nothing reusable - it will ask again)".to_string(),
            ),
        };
        Self {
            id: id.into(),
            kind: InteractionKind::ToolApproval,
            prompt,
            options: vec![
                "Allow once".to_string(),
                stage_label,
                run_label,
                "Deny".to_string(),
                DENY_WITH_FEEDBACK.to_string(),
            ],
            tool_name: Some(tool),
            tool_arguments: Some(arguments),
            required: true,
            stage_name: stage.into(),
            body: None,
            body_format: BodyFormat::Plain,
        }
    }

    /// Whether option `index` of this request is the deny that takes a
    /// message for the model.
    ///
    /// A client that offers it has to open a text box before answering, so it
    /// needs to know which row that is. Keyed on the label rather than a fixed
    /// position because the taint gate's approval has no such row, and a
    /// client that assumed index four on every approval would open the box on
    /// a request that cannot carry the text.
    pub fn is_deny_with_feedback(&self, index: usize) -> bool {
        self.kind == InteractionKind::ToolApproval
            && self.options.get(index).map(String::as_str) == Some(DENY_WITH_FEEDBACK)
    }
}

/// The label of the tool-approval option that denies and tells the model why.
///
/// The plain "Deny" hands the model nothing but the refusal, and its next turn
/// is a guess at what it should have done instead. This one carries a line
/// from the person into the tool result, so the next turn is a redirect. Every
/// client renders it from the request's `options`, so this is where the words
/// live.
pub const DENY_WITH_FEEDBACK: &str = "Deny with feedback";

/// The scope a tool-approval option index means, or `None` for deny.
///
/// One definition, so the dashboard, `lev respond`, the REST endpoint and the
/// ACP bridge cannot drift from the labels [`InteractionRequest::tool_approval`]
/// builds. An index past the end denies: an answer this does not recognise must
/// never approve.
pub fn approval_choice(index: usize) -> Option<ApprovalScope> {
    match index {
        0 => Some(ApprovalScope::Once),
        1 => Some(ApprovalScope::Stage),
        2 => Some(ApprovalScope::Run),
        _ => None,
    }
}

// ─── Response ───────────────────────────────────────────────────────────────

/// How far an approval reaches.
///
/// The three scopes are what a person actually wants to say. `Once` is "I read
/// this one". `Stage` is "keep going through the work I just approved", which
/// expires when the run moves on to different work. `Run` is "I trust this for
/// the whole task". Nothing persists past the run: a grant written to disk
/// would outlive the reason the user made it.
///
/// What a grant covers is a set of keys derived from the call, not the tool
/// name - see `leviath_cli::shell_keys`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalScope {
    /// Just this one call.
    Once,
    /// Every later call this covers, until the run leaves the current stage.
    Stage,
    /// Every later call this covers, for the rest of the run.
    ///
    /// Serialized as `session`, which is the name every client already sends:
    /// `lev respond --session`, the REST `"scope": "session"`, and the ACP
    /// `allow-always` option all mean this.
    #[serde(rename = "session", alias = "run")]
    Run,
}

/// A response written by the dashboard (or `lev respond`) to answer the worker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    /// What the person wants the model to do instead, on a denied tool
    /// approval. Reaches the model as part of the tool result.
    ///
    /// Absent (the default) is the plain deny every existing client sends, so
    /// an answer written before this field existed still means what it meant.
    /// Only ever read alongside `approved: Some(false)`: a grant has nothing to
    /// redirect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<String>,
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
            feedback: None,
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
            feedback: None,
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
            feedback: None,
        }
    }

    /// Build a deny that tells the model what to do instead.
    ///
    /// The text is trimmed, and an answer that is all whitespace is the plain
    /// deny: the tool result must not carry an empty "Feedback:" for the model
    /// to puzzle over.
    pub fn deny_with_feedback(request_id: impl Into<String>, feedback: &str) -> Self {
        let feedback = feedback.trim();
        Self {
            request_id: request_id.into(),
            value: None,
            choice_index: None,
            approved: Some(false),
            scope: Some(ApprovalScope::Once),
            feedback: (!feedback.is_empty()).then(|| feedback.to_string()),
        }
    }

    /// The redirect a denied tool approval carries, if the person wrote one.
    ///
    /// `None` on a grant, whatever the field says: feedback beside
    /// `approved: true` is a client bug, and the model must not be told the
    /// call it just ran was refused.
    pub fn deny_feedback(&self) -> Option<&str> {
        match self.approved {
            Some(false) => self
                .feedback
                .as_deref()
                .map(str::trim)
                .filter(|f| !f.is_empty()),
            _ => None,
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

    /// "Allow tool call: `bash`?" asks whether to run a shell command without
    /// saying which one - the only safe answer is no and the only practical one
    /// is yes, so in practice everything gets approved unread.
    #[test]
    fn a_tool_approval_says_what_it_is_asking_about() {
        let req = InteractionRequest::tool_approval(
            "id",
            "bash",
            serde_json::json!({"command": "rm -rf build && make"}),
            "implement",
            &[],
        );
        assert!(
            req.prompt.contains("rm -rf build && make"),
            "{}",
            req.prompt
        );

        // File tools name the path.
        let req = InteractionRequest::tool_approval(
            "id",
            "write_file",
            serde_json::json!({"path": "src/main.rs", "content": "..."}),
            "implement",
            &[],
        );
        assert!(req.prompt.contains("src/main.rs"), "{}", req.prompt);
    }

    /// A prompt is one line in a terminal. A heredoc that scrolls the decision
    /// off screen is the same problem as showing nothing.
    #[test]
    fn a_long_or_multiline_command_is_summarised() {
        let long = "echo ".to_string() + &"x".repeat(400);
        let req = InteractionRequest::tool_approval(
            "id",
            "bash",
            serde_json::json!({ "command": long }),
            "s",
            &[],
        );
        assert!(req.prompt.chars().count() < 200, "{}", req.prompt);
        assert!(req.prompt.contains('…'), "{}", req.prompt);

        let req = InteractionRequest::tool_approval(
            "id",
            "bash",
            serde_json::json!({"command": "cat <<'EOF' > f\nline two\nEOF"}),
            "s",
            &[],
        );
        assert!(req.prompt.contains("cat <<'EOF' > f"), "{}", req.prompt);
        assert!(!req.prompt.contains("line two"), "{}", req.prompt);
        // The whole thing is still available to a richer UI.
        assert!(req.tool_arguments.is_some());
    }

    /// A tool with no argument worth showing keeps the plain question rather
    /// than gaining an empty pair of backticks.
    #[test]
    fn a_tool_without_a_telling_argument_reads_as_before() {
        let req =
            InteractionRequest::tool_approval("id", "list_dir", serde_json::json!({}), "s", &[]);
        assert_eq!(req.prompt, "Allow tool call: `list_dir`?");
        let req = InteractionRequest::tool_approval(
            "id",
            "bash",
            serde_json::json!({"command": "   "}),
            "s",
            &[],
        );
        assert_eq!(req.prompt, "Allow tool call: `bash`?");
        // Present but not a string: still nothing worth showing.
        let req = InteractionRequest::tool_approval(
            "id",
            "bash",
            serde_json::json!({"command": 42}),
            "s",
            &[],
        );
        assert_eq!(req.prompt, "Allow tool call: `bash`?");
    }

    /// "Allow for this session" said nothing about what the session would then
    /// be allowed to do, and the dispatcher silently degraded it to once when
    /// the call had no reusable key - so the user chose a grant they did not
    /// get. The labels now name it.
    #[test]
    fn the_scope_options_name_what_they_grant() {
        let keys = |names: &[&str]| -> Vec<String> {
            names.iter().map(|n| format!("shell:{n}")).collect()
        };
        let req = InteractionRequest::tool_approval(
            "id",
            "shell",
            serde_json::json!({"command": "ls && git status"}),
            "s",
            &keys(&["git status", "ls"]),
        );
        assert_eq!(req.options[1], "Allow git status, ls for this stage");
        assert_eq!(req.options[2], "Allow git status, ls for this run");

        // Beyond three, the rest are counted rather than wrapped off screen.
        let req = InteractionRequest::tool_approval(
            "id",
            "shell",
            serde_json::json!({}),
            "s",
            &keys(&["a", "b", "c", "d", "e"]),
        );
        assert_eq!(req.options[2], "Allow a, b, c +2 more for this run");

        // A non-shell key is a bare tool name, with no prefix to strip.
        let req = InteractionRequest::tool_approval(
            "id",
            "web_fetch",
            serde_json::json!({}),
            "s",
            &["web_fetch".to_string()],
        );
        assert_eq!(req.options[2], "Allow web_fetch for this run");
    }

    /// A call with no reusable key says so, rather than offering a grant the
    /// dispatcher will not record.
    #[test]
    fn an_unkeyable_call_says_it_will_ask_again() {
        let req = InteractionRequest::tool_approval(
            "id",
            "shell",
            serde_json::json!({"command": "echo `whoami`"}),
            "s",
            &[],
        );
        assert!(
            req.options[1].contains("it will ask again"),
            "{:?}",
            req.options
        );
        assert!(
            req.options[2].contains("it will ask again"),
            "{:?}",
            req.options
        );
    }

    /// The index-to-scope mapping is what every client uses, so it has to match
    /// the option order exactly, and an index it does not recognise must deny.
    #[test]
    fn approval_choice_matches_the_option_order() {
        let req = InteractionRequest::tool_approval("id", "shell", serde_json::json!({}), "s", &[]);
        assert_eq!(approval_choice(0), Some(ApprovalScope::Once));
        assert!(req.options[1].contains("stage"));
        assert_eq!(approval_choice(1), Some(ApprovalScope::Stage));
        assert!(req.options[2].contains("run"));
        assert_eq!(approval_choice(2), Some(ApprovalScope::Run));
        assert_eq!(req.options[3], "Deny");
        assert_eq!(approval_choice(3), None);
        assert_eq!(req.options[4], DENY_WITH_FEEDBACK);
        assert_eq!(approval_choice(4), None, "feedback is still a deny");
        assert_eq!(
            approval_choice(99),
            None,
            "an unknown answer must not approve"
        );
    }

    /// A gate approval is a different decision, so it keeps its own wording and
    /// offers no per-stage scope: clearance is not keyed on what a call runs.
    #[test]
    fn a_gate_approval_offers_run_scope_and_no_stage_scope() {
        let req =
            InteractionRequest::gate_approval("g", "web_fetch", serde_json::json!({}), "research");
        assert_eq!(req.kind, InteractionKind::ToolApproval);
        assert_eq!(req.prompt, "Allow tool call: `web_fetch`?");
        assert_eq!(req.options, ["Allow once", "Allow for this run", "Deny"]);
    }

    /// `session` stays the wire name for run scope: every client already sends
    /// it, and renaming it on the wire would silently narrow their grants.
    #[test]
    fn run_scope_serialises_as_session() {
        assert_eq!(
            serde_json::to_string(&ApprovalScope::Run).unwrap(),
            "\"session\""
        );
        for wire in ["\"session\"", "\"run\""] {
            let back: ApprovalScope = serde_json::from_str(wire).unwrap();
            assert_eq!(back, ApprovalScope::Run, "{wire}");
        }
        assert_eq!(
            serde_json::to_string(&ApprovalScope::Stage).unwrap(),
            "\"stage\""
        );
    }

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
            &[],
        );
        assert_eq!(r.kind, InteractionKind::ToolApproval);
        assert_eq!(r.options.len(), 5);
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

        let r = InteractionResponse::approval("id3", true, ApprovalScope::Run);
        assert_eq!(r.approved, Some(true));
        assert_eq!(r.scope, Some(ApprovalScope::Run));
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
            feedback: None,
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
        let r = InteractionRequest::tool_approval("ta1", "write_file", args, "code", &[]);
        assert_eq!(r.kind, InteractionKind::ToolApproval);
        assert_eq!(r.tool_name.as_deref(), Some("write_file"));
        assert!(r.tool_arguments.is_some());
        assert_eq!(r.options.len(), 5);
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
        let r = InteractionResponse::approval("id", true, ApprovalScope::Run);
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
        for scope in [ApprovalScope::Once, ApprovalScope::Run] {
            let json = serde_json::to_string(&scope).unwrap();
            let back: ApprovalScope = serde_json::from_str(&json).unwrap();
            assert_eq!(scope, back);
        }
    }

    #[test]
    fn test_approval_scope_snake_case() {
        let json = serde_json::to_string(&ApprovalScope::Once).unwrap();
        assert_eq!(json, "\"once\"");
        let json = serde_json::to_string(&ApprovalScope::Run).unwrap();
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
            &[],
        );
        let json = serde_json::to_string(&req).unwrap();
        let back: InteractionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "serde1");
        assert_eq!(back.kind, InteractionKind::ToolApproval);
        assert_eq!(back.tool_name.as_deref(), Some("bash"));
    }

    #[test]
    fn test_interaction_response_serde_roundtrip() {
        let resp = InteractionResponse::approval("serde2", true, ApprovalScope::Run);
        let json = serde_json::to_string(&resp).unwrap();
        let back: InteractionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.request_id, "serde2");
        assert_eq!(back.approved, Some(true));
        assert_eq!(back.scope, Some(ApprovalScope::Run));
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
        let r =
            InteractionRequest::tool_approval("ta", "bash", serde_json::json!({}), "stage", &[]);
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
        let resp = InteractionResponse::approval("a1", false, ApprovalScope::Run);
        let json = serde_json::to_string(&resp).unwrap();
        let back: InteractionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.approved, Some(false));
        assert_eq!(back.scope, Some(ApprovalScope::Run));
    }

    #[test]
    fn test_response_as_text_with_value() {
        let r = InteractionResponse::text("id", "some text value");
        assert_eq!(response_as_text(&r), "some text value");
    }

    #[test]
    fn test_response_approved_session_scope() {
        let r = InteractionResponse::approval("id", true, ApprovalScope::Run);
        assert!(response_approved(&r));
        assert_eq!(r.scope, Some(ApprovalScope::Run));
    }

    #[test]
    fn test_make_interaction_id_various() {
        assert_eq!(make_interaction_id(1, 2), "1-2");
        assert_eq!(make_interaction_id(10, 20), "10-20");
    }

    #[test]
    fn test_default_true_via_serde_missing_required_field() {
        // JSON without `required` - should default to true via default_true()
        let json = r#"{
            "id": "dt1",
            "kind": "free_text",
            "prompt": "test prompt",
            "stage_name": "stage"
        }"#;
        let req: InteractionRequest = serde_json::from_str(json).unwrap();
        assert!(req.required);
    }
    // ─── deny with feedback ──────────────────────────────────────────────────

    /// The wire shape every existing client sends has no `feedback` key, and
    /// the shape a new one sends adds exactly that key and nothing else.
    #[test]
    fn feedback_is_absent_from_the_wire_unless_given() {
        let plain = InteractionResponse::approval("q1", false, ApprovalScope::Once);
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json.get("feedback").is_none(), "{json}");
        let old_wire: InteractionResponse =
            serde_json::from_str(r#"{"request_id":"q1","value":null,"choice_index":null,"approved":false,"scope":"once"}"#)
                .unwrap();
        assert_eq!(old_wire, plain);

        let with = InteractionResponse::deny_with_feedback("q1", "  use git log instead \n");
        let json = serde_json::to_value(&with).unwrap();
        assert_eq!(json["feedback"], "use git log instead");
        assert_eq!(json["approved"], false);
        let back: InteractionResponse = serde_json::from_value(json).unwrap();
        assert_eq!(back, with);
        assert_eq!(back.deny_feedback(), Some("use git log instead"));
    }

    /// Whitespace is not a message: the constructor and the reader both turn
    /// it into the plain deny.
    #[test]
    fn blank_feedback_is_the_plain_deny() {
        let blank = InteractionResponse::deny_with_feedback("q1", "   \n\t");
        assert_eq!(blank.feedback, None);
        assert_eq!(blank.approved, Some(false));
        assert_eq!(blank.deny_feedback(), None);
        let padded = InteractionResponse {
            feedback: Some("  \n ".to_string()),
            ..InteractionResponse::approval("q1", false, ApprovalScope::Once)
        };
        assert_eq!(padded.deny_feedback(), None);
    }

    /// Feedback beside a grant, or beside no decision at all, is never read.
    #[test]
    fn feedback_is_only_read_on_a_deny() {
        let granted = InteractionResponse {
            feedback: Some("why".to_string()),
            ..InteractionResponse::approval("q1", true, ApprovalScope::Run)
        };
        assert_eq!(granted.deny_feedback(), None);
        let undecided = InteractionResponse {
            feedback: Some("why".to_string()),
            ..InteractionResponse::text("q1", "")
        };
        assert_eq!(undecided.deny_feedback(), None);
    }

    /// Only a tool approval has the feedback row, and only at the position
    /// the label sits at.
    #[test]
    fn the_feedback_row_is_found_by_label_on_tool_approvals_only() {
        let tool =
            InteractionRequest::tool_approval("id", "shell", serde_json::json!({}), "s", &[]);
        assert!(tool.is_deny_with_feedback(4));
        assert!(!tool.is_deny_with_feedback(3));
        assert!(!tool.is_deny_with_feedback(99));
        let gate = InteractionRequest::gate_approval("id", "web_fetch", serde_json::json!({}), "s");
        assert!(!gate.is_deny_with_feedback(2));
        assert!(!gate.is_deny_with_feedback(4));
        let choice = InteractionRequest::multiple_choice(
            "id",
            "?",
            vec![DENY_WITH_FEEDBACK.to_string()],
            "s",
        );
        assert!(
            !choice.is_deny_with_feedback(0),
            "the label alone is not the row"
        );
    }
}
