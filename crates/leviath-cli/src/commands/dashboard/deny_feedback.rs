//! The "Deny with feedback" path of a tool-approval prompt.
//!
//! The prompt is a list of choices; the plain choices answer on Enter. This
//! one cannot, because the answer is a deny plus a message the model will read
//! as its redirect, and the message has to be typed first. So Enter on that
//! row opens the same long-form response box the free-text answers use, and
//! sending from it answers with [`InteractionResponse::deny_with_feedback`].
//! Esc in the box goes back to the prompt with the other choices still there,
//! not out of input mode: a person who changes their mind about explaining is
//! not a person who changes their mind about answering.

use leviath_core::interaction::InteractionResponse;

use super::helpers::truncate;
use super::state::Dashboard;
use crate::tui::widgets::markdown_edit::MarkdownEdit;

impl Dashboard {
    /// Enter on a choice prompt: answer it, unless the highlighted row is the
    /// deny that needs a message first, which opens the box instead.
    pub(super) fn confirm_choice(&mut self) {
        let opens_box = self
            .selected_agent()
            .and_then(|a| a.pending_request.as_ref())
            .is_some_and(|r| r.is_deny_with_feedback(self.choice_selected));
        if opens_box {
            self.open_deny_feedback_box();
        } else {
            self.submit_input();
        }
    }

    /// Open the response box under the approval prompt, empty and in the
    /// person's preferred markdown mode, to collect the deny's message.
    fn open_deny_feedback_box(&mut self) {
        let mode = self.md_mode();
        self.input_textarea = MarkdownEdit::default().in_mode(mode);
        self.response_focus_send = false;
        self.deny_feedback_open = true;
    }

    /// Esc in the response box. Under an approval prompt that is "back to the
    /// choices", with the highlighted row where it was; anywhere else it is
    /// what it always was, out of input mode with nothing sent.
    pub(super) fn cancel_response_box(&mut self) {
        if self.deny_feedback_open {
            self.deny_feedback_open = false;
            self.response_focus_send = false;
            self.input_textarea = MarkdownEdit::default();
        } else {
            self.close_input_box();
        }
    }

    /// The answer the open feedback box sends for request `id`, and the label
    /// the activity log shows for it. Internal newlines are kept: the model
    /// reads the text as written.
    pub(super) fn deny_feedback_response(&self, id: &str) -> (InteractionResponse, String) {
        let text = self.input_textarea.lines().join("\n");
        let response = InteractionResponse::deny_with_feedback(id, &text);
        let label = match response.feedback.as_deref() {
            Some(feedback) => format!("Deny: {}", truncate(feedback, 34)),
            None => "Deny".to_string(),
        };
        (response, label)
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use leviath_core::interaction::{ApprovalScope, InteractionRequest, InteractionResponse};
    use tokio::sync::mpsc;

    use super::super::state::Dashboard;
    use super::super::types::{AgentDisplayStatus, DaemonCommand, DashboardAgent};

    fn agent_with_approval(id: &str) -> DashboardAgent {
        DashboardAgent {
            id: id.to_string(),
            blueprint_name: "probe".to_string(),
            stage: "main".to_string(),
            stage_index: 0,
            num_stages: 1,
            status: AgentDisplayStatus::Waiting,
            tokens_in: 0,
            tokens_out: 0,
            cached_tokens: 0,
            iteration: 1,
            broken_scripts: Vec::new(),
            waiting_prompt: Some("Allow tool call: `bash`?".to_string()),
            wait_reason: None,
            pending_request: Some(InteractionRequest::tool_approval(
                "ta1",
                "bash",
                serde_json::json!({"command": "rm -rf build"}),
                "main",
                &["shell:rm".to_string()],
            )),
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
            workdir: "/tmp/test".to_string(),
            task: "task".to_string(),
            title: None,
            model: None,
            parent_id: None,
            started_at: 0,
            last_progress_at: None,
            runtime_secs: 0,
            clock_now: 0,
            graph: None,
            accepts_messages: true,
        }
    }

    /// A dashboard on the approval prompt of one waiting run, in input mode,
    /// with the deny-with-feedback row highlighted.
    fn on_the_prompt() -> (Dashboard, mpsc::UnboundedReceiver<DaemonCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        dash.agents.push(agent_with_approval("run-1"));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;
        dash.choice_selected = 4;
        (dash, cmd_rx)
    }

    fn press(dash: &mut Dashboard, code: KeyCode, modifiers: KeyModifiers) {
        dash.handle_input_mode_key(code, KeyEvent::new(code, modifiers));
    }

    fn type_text(dash: &mut Dashboard, text: &str) {
        for ch in text.chars() {
            press(dash, KeyCode::Char(ch), KeyModifiers::NONE);
        }
    }

    /// The whole path: Enter on the row opens the box instead of answering,
    /// two typed lines, Ctrl+Enter sends a deny carrying both lines, and the
    /// prompt is gone.
    #[test]
    fn enter_on_the_row_opens_the_box_and_ctrl_enter_sends_the_deny() {
        let (mut dash, mut cmd_rx) = on_the_prompt();
        press(&mut dash, KeyCode::Enter, KeyModifiers::NONE);
        assert!(dash.deny_feedback_open, "the box opened");
        assert!(dash.input_mode, "still in input mode");
        assert!(cmd_rx.try_recv().is_err(), "nothing was answered yet");

        type_text(&mut dash, "keep build/");
        press(&mut dash, KeyCode::Enter, KeyModifiers::NONE);
        type_text(&mut dash, "clean only dist/");
        assert_eq!(
            dash.input_textarea.lines().len(),
            2,
            "Enter is a newline in the box"
        );
        assert!(cmd_rx.try_recv().is_err(), "Enter did not send");

        press(&mut dash, KeyCode::Enter, KeyModifiers::CONTROL);
        assert_eq!(
            cmd_rx.try_recv().expect("the deny was sent"),
            DaemonCommand::Answer {
                response: InteractionResponse::deny_with_feedback(
                    "ta1",
                    "keep build/\nclean only dist/"
                ),
            }
        );
        assert!(!dash.input_mode);
        assert!(!dash.deny_feedback_open);
        assert!(dash.agents[0].pending_request.is_none());
        assert_eq!(dash.choice_selected, 0);
    }

    /// The Send button under the box sends the same answer, for a terminal
    /// that cannot tell Ctrl+Enter from Enter.
    #[test]
    fn tab_then_enter_sends_from_the_button() {
        let (mut dash, mut cmd_rx) = on_the_prompt();
        press(&mut dash, KeyCode::Enter, KeyModifiers::NONE);
        type_text(&mut dash, "use the API");
        press(&mut dash, KeyCode::Tab, KeyModifiers::NONE);
        assert!(dash.response_focus_send);
        press(&mut dash, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            cmd_rx.try_recv().expect("sent from the button"),
            DaemonCommand::Answer {
                response: InteractionResponse::deny_with_feedback("ta1", "use the API"),
            }
        );
    }

    /// Esc in the box is "back to the prompt": input mode stays, the choices
    /// are back, the highlighted row is unchanged, and nothing was answered.
    /// A second Esc, on the prompt, leaves input mode as it always did.
    #[test]
    fn esc_returns_to_the_prompt_without_answering() {
        let (mut dash, mut cmd_rx) = on_the_prompt();
        press(&mut dash, KeyCode::Enter, KeyModifiers::NONE);
        type_text(&mut dash, "half a thou");
        press(&mut dash, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!dash.deny_feedback_open);
        assert!(dash.input_mode, "back on the prompt, not out of it");
        assert_eq!(dash.choice_selected, 4);
        assert!(
            dash.input_textarea.lines().concat().is_empty(),
            "the draft is gone"
        );
        assert!(cmd_rx.try_recv().is_err());
        assert!(dash.agents[0].pending_request.is_some());

        // The same Esc from the Send button.
        press(&mut dash, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut dash, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut dash, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!dash.deny_feedback_open);
        assert!(dash.input_mode);

        press(&mut dash, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!dash.input_mode, "Esc on the prompt leaves input mode");
    }

    /// Sending an empty box is the plain deny: the model must not read an
    /// empty "Feedback:".
    #[test]
    fn an_empty_box_sends_the_plain_deny() {
        let (mut dash, mut cmd_rx) = on_the_prompt();
        press(&mut dash, KeyCode::Enter, KeyModifiers::NONE);
        type_text(&mut dash, "   ");
        press(&mut dash, KeyCode::Enter, KeyModifiers::CONTROL);
        assert_eq!(
            cmd_rx.try_recv().expect("sent"),
            DaemonCommand::Answer {
                response: InteractionResponse::approval("ta1", false, ApprovalScope::Once),
            }
        );
    }

    /// Enter on any other row still answers at once, as it always did.
    #[test]
    fn enter_on_the_plain_deny_answers_without_a_box() {
        let (mut dash, mut cmd_rx) = on_the_prompt();
        dash.choice_selected = 3;
        press(&mut dash, KeyCode::Enter, KeyModifiers::NONE);
        assert!(!dash.deny_feedback_open);
        assert_eq!(
            cmd_rx.try_recv().expect("answered"),
            DaemonCommand::Answer {
                response: InteractionResponse::approval("ta1", false, ApprovalScope::Once),
            }
        );
    }

    /// The activity-log label names the deny and quotes the feedback, cut to
    /// fit the log line.
    #[test]
    fn the_log_label_quotes_the_feedback() {
        let (mut dash, _rx) = on_the_prompt();
        press(&mut dash, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(dash.deny_feedback_response("ta1").1, "Deny");
        type_text(&mut dash, "short");
        assert_eq!(dash.deny_feedback_response("ta1").1, "Deny: short");
        type_text(&mut dash, &"x".repeat(60));
        let label = dash.deny_feedback_response("ta1").1;
        assert!(label.starts_with("Deny: short"), "{label}");
        assert!(label.len() < 50, "{label}");
    }
}
