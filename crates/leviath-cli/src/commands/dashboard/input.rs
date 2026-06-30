//! Keyboard event handling, input submission, and event processing.

use crossterm::event::KeyCode;

use super::helpers::{kill_write_cancelled, truncate};
use super::state::Dashboard;
use super::types::*;
use crate::interaction;
use crate::runstate;

impl Dashboard {
    pub(super) fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        let key_code = key.code;
        // Help overlay takes priority
        if self.show_help {
            self.show_help = false;
            return;
        }

        // Delete confirmation popup has highest priority
        if self.confirm_delete {
            match key_code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.confirm_delete = false;
                    self.delete_selected_agent();
                }
                _ => {
                    self.confirm_delete = false;
                    self.add_log("Delete cancelled".to_string());
                }
            }
            return;
        }

        // ── Detail view ─────────────────────────────────────────────────────
        if self.detail_view {
            if self.input_mode {
                self.handle_input_mode_key(key_code, key);
                return;
            }

            // Search mode: intercept all keys for query editing
            if self.search_mode {
                match key_code {
                    KeyCode::Esc | KeyCode::Enter => {
                        if key_code == KeyCode::Esc {
                            self.search_query.clear();
                            self.search_match_idx = 0;
                        }
                        self.search_mode = false;
                    }
                    KeyCode::Backspace => {
                        self.search_query.pop();
                        self.search_match_idx = 0;
                    }
                    KeyCode::Char(c) => {
                        self.search_query.push(c);
                        self.search_match_idx = 0;
                    }
                    _ => {}
                }
                return;
            }

            // Detail view — not in input mode
            self.handle_detail_view_key(key_code);
            return;
        }

        // ── Main agent list ──────────────────────────────────────────────────
        // ── Main list filter mode: intercept all keys for query editing ─────────
        if self.list_search_mode {
            match key_code {
                KeyCode::Esc => {
                    self.list_search_mode = false;
                    self.list_search_query.clear();
                    self.selected = 0;
                    self.update_display_indices();
                }
                KeyCode::Enter => {
                    self.list_search_mode = false;
                }
                KeyCode::Backspace => {
                    self.list_search_query.pop();
                    self.selected = 0;
                    self.update_display_indices();
                }
                KeyCode::Char(c) => {
                    self.list_search_query.push(c);
                    self.selected = 0;
                    self.update_display_indices();
                }
                _ => {}
            }
            return;
        }

        self.handle_main_list_key(key_code);
    }

    fn handle_input_mode_key(&mut self, key_code: KeyCode, key: crossterm::event::KeyEvent) {
        use interaction::InteractionKind;
        let kind = self
            .selected_agent()
            .and_then(|a| a.pending_request.as_ref())
            .map(|r| r.kind.clone());
        let options_len = self
            .selected_agent()
            .and_then(|a| a.pending_request.as_ref())
            .map(|r| r.options.len())
            .unwrap_or(0);

        match &kind {
            Some(InteractionKind::FreeText) | None => match key_code {
                KeyCode::Enter if key.modifiers.is_empty() => {
                    self.submit_input();
                }
                KeyCode::Esc => {
                    self.input_mode = false;
                    self.input_textarea = tui_textarea::TextArea::default();
                    self.choice_selected = 0;
                }
                _ => {
                    self.input_textarea.input(tui_textarea::Input::from(key));
                }
            },
            _ => match key_code {
                KeyCode::Esc => {
                    self.input_mode = false;
                    self.input_textarea = tui_textarea::TextArea::default();
                    self.choice_selected = 0;
                }
                KeyCode::Enter => {
                    self.submit_input();
                }
                KeyCode::Up => {
                    if self.choice_selected > 0 {
                        self.choice_selected -= 1;
                    }
                }
                KeyCode::Down if options_len > 0 && self.choice_selected < options_len - 1 => {
                    self.choice_selected += 1;
                }
                _ => {}
            },
        }
    }

    fn handle_detail_view_key(&mut self, key_code: KeyCode) {
        match key_code {
            KeyCode::Esc => {
                if !self.search_query.is_empty() {
                    // First Esc clears the search; second exits detail view
                    self.search_query.clear();
                    self.search_match_idx = 0;
                } else {
                    self.detail_view = false;
                    self.detail_scroll = 0;
                    self.review_scroll = 0;
                }
            }
            // Stage tab navigation
            KeyCode::Left => {
                if self.selected_stage > 0 {
                    self.selected_stage -= 1;
                    self.detail_scroll = 0;
                    self.review_scroll = 0;
                    self.search_mode = false;
                    self.search_query.clear();
                    self.search_match_idx = 0;
                }
            }
            KeyCode::Right => {
                let max_stage = self
                    .selected_agent()
                    .map(|a| a.num_stages.saturating_sub(1))
                    .unwrap_or(0);
                if self.selected_stage < max_stage {
                    self.selected_stage += 1;
                    self.detail_scroll = 0;
                    self.review_scroll = 0;
                    self.search_mode = false;
                    self.search_query.clear();
                    self.search_match_idx = 0;
                }
            }
            // Number keys 1-9: jump to stage tab
            KeyCode::Char(c @ '1'..='9') => {
                let idx = (c as usize) - ('1' as usize);
                let max_stage = self
                    .selected_agent()
                    .map(|a| a.num_stages.saturating_sub(1))
                    .unwrap_or(0);
                if idx <= max_stage {
                    self.selected_stage = idx;
                    self.detail_scroll = 0;
                    self.review_scroll = 0;
                    self.search_mode = false;
                    self.search_query.clear();
                    self.search_match_idx = 0;
                }
            }
            // Content mode toggle
            KeyCode::Char('l') => {
                self.stage_content_mode = StageContentMode::Logs;
                self.detail_scroll = 0;
            }
            KeyCode::Char('o') => {
                self.stage_content_mode = StageContentMode::Output;
                self.detail_scroll = 0;
            }
            KeyCode::Char('c') => {
                self.stage_content_mode = StageContentMode::Context;
                self.detail_scroll = 0;
            }
            KeyCode::Char('i') => {
                if self.selected_stage_can_respond() {
                    self.input_mode = true;
                    self.choice_selected = 0;
                    self.input_textarea = tui_textarea::TextArea::default();
                }
            }
            KeyCode::Char('m') => {
                // Send a mid-run message to any active agent (regardless of interaction state)
                if self.selected_agent_accepts_messages() {
                    self.input_mode = true;
                    self.choice_selected = 0;
                    self.input_textarea = tui_textarea::TextArea::default();
                }
            }
            KeyCode::Up => {
                // When a review body is present, Up scrolls the review document
                let has_review = self
                    .selected_agent()
                    .and_then(|a| a.pending_request.as_ref())
                    .and_then(|r| r.body.as_deref())
                    .map(|b| !b.is_empty())
                    .unwrap_or(false);
                if has_review {
                    self.review_scroll = self.review_scroll.saturating_add(1);
                } else {
                    self.detail_scroll = self.detail_scroll.saturating_add(1);
                }
            }
            KeyCode::Down => {
                let has_review = self
                    .selected_agent()
                    .and_then(|a| a.pending_request.as_ref())
                    .and_then(|r| r.body.as_deref())
                    .map(|b| !b.is_empty())
                    .unwrap_or(false);
                if has_review {
                    self.review_scroll = self.review_scroll.saturating_sub(1);
                } else {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                }
            }
            KeyCode::PageUp => {
                let has_review = self
                    .selected_agent()
                    .and_then(|a| a.pending_request.as_ref())
                    .and_then(|r| r.body.as_deref())
                    .map(|b| !b.is_empty())
                    .unwrap_or(false);
                if has_review {
                    self.review_scroll = self.review_scroll.saturating_add(10);
                } else {
                    self.detail_scroll = self.detail_scroll.saturating_add(10);
                }
            }
            KeyCode::PageDown => {
                let has_review = self
                    .selected_agent()
                    .and_then(|a| a.pending_request.as_ref())
                    .and_then(|r| r.body.as_deref())
                    .map(|b| !b.is_empty())
                    .unwrap_or(false);
                if has_review {
                    self.review_scroll = self.review_scroll.saturating_sub(10);
                } else {
                    self.detail_scroll = self.detail_scroll.saturating_sub(10);
                }
            }
            KeyCode::Char('b') => {
                self.detail_scroll = usize::MAX;
                self.review_scroll = usize::MAX;
            }
            KeyCode::Char('e') => {
                self.detail_scroll = 0;
                self.review_scroll = 0;
            }
            KeyCode::Char('?') => {
                self.show_help = true;
            }
            // Search: `/` enters search mode; `n`/`N` step through matches
            KeyCode::Char('/') => {
                self.search_mode = true;
                self.search_query.clear();
                self.search_match_idx = 0;
            }
            KeyCode::Char('n') => {
                if !self.search_query.is_empty() {
                    self.search_match_idx = self.search_match_idx.saturating_add(1);
                }
            }
            KeyCode::Char('N') => {
                if !self.search_query.is_empty() {
                    self.search_match_idx = self.search_match_idx.saturating_sub(1);
                }
            }
            // Yank: `y` copies the current stage content to clipboard
            KeyCode::Char('y') => {
                self.handle_yank();
            }
            KeyCode::Char('k') => {
                self.handle_kill_from_detail();
            }
            _ => {}
        }
    }

    fn handle_yank(&mut self) {
        use super::helpers::yank_to_clipboard;

        if let Some(agent) = self.selected_agent() {
            if agent.is_run_state {
                let (content, label) = match self.stage_content_mode {
                    StageContentMode::Output => (
                        runstate::tail_stage_output(&agent.id, self.selected_stage, 524_288),
                        "Output",
                    ),
                    StageContentMode::Logs => (
                        runstate::tail_stage_log(&agent.id, self.selected_stage, 524_288),
                        "Logs",
                    ),
                    StageContentMode::Context => {
                        let json = std::fs::read_to_string(
                            runstate::stage_dir(&agent.id, self.selected_stage)
                                .join("context.json"),
                        )
                        .unwrap_or_default();
                        (json, "Context JSON")
                    }
                };
                if content.is_empty() {
                    self.toasts.push(Toast {
                        message: format!("No {} content to yank", label),
                        remaining_ticks: 25,
                        level: ToastLevel::Warning,
                    });
                } else if yank_to_clipboard(&content) {
                    self.toasts.push(Toast {
                        message: format!("{} yanked to clipboard", label),
                        remaining_ticks: 25,
                        level: ToastLevel::Info,
                    });
                } else {
                    self.toasts.push(Toast {
                        message: "Clipboard unavailable (no pbcopy/xclip/OSC52)".to_string(),
                        remaining_ticks: 30,
                        level: ToastLevel::Error,
                    });
                }
            }
        }
    }

    fn handle_kill_from_detail(&mut self) {
        if let Some(agent) = self.selected_agent() {
            if matches!(
                agent.status,
                AgentDisplayStatus::Active | AgentDisplayStatus::Waiting
            ) {
                let agent_id = agent.id.clone();
                let _pid = agent.pid;
                let is_run_state = agent.is_run_state;
                let was_waiting = matches!(agent.status, AgentDisplayStatus::Waiting);
                if is_run_state {
                    #[cfg(unix)]
                    if _pid > 0 {
                        unsafe {
                            libc::kill(_pid as libc::pid_t, libc::SIGTERM);
                        }
                    }
                    kill_write_cancelled(&agent_id);
                    if was_waiting {
                        interaction::clear_interaction(&agent_id);
                    }
                } else {
                    let _ = self.cmd_tx.send(EngineCommand::CancelAgent {
                        agent_id: agent_id.clone(),
                    });
                }
                if let Some(a) = self.selected_agent_mut() {
                    a.status = AgentDisplayStatus::Cancelled;
                    a.waiting_prompt = None;
                    a.pending_request = None;
                }
                self.input_mode = false;
                self.input_textarea = tui_textarea::TextArea::default();
                self.add_log(format!("{}: Killed", agent_id));
            }
        }
    }

    fn handle_main_list_key(&mut self, key_code: KeyCode) {
        match key_code {
            KeyCode::Esc => {
                if !self.list_search_query.is_empty() {
                    // First Esc clears the filter; second exits (quit)
                    self.list_search_query.clear();
                    self.selected = 0;
                    self.update_display_indices();
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::Up => {
                if !self.display_indices.is_empty() && self.selected > 0 {
                    self.selected -= 1;
                    self.table_state.select(Some(self.selected));
                }
            }
            KeyCode::Down => {
                if !self.display_indices.is_empty()
                    && self.selected < self.display_indices.len() - 1
                {
                    self.selected += 1;
                    self.table_state.select(Some(self.selected));
                }
            }
            KeyCode::Enter => {
                if !self.display_indices.is_empty() {
                    self.detail_view = true;
                    self.detail_scroll = 0;
                    // Default to the currently active stage when opening detail view
                    self.selected_stage = self.selected_agent().map(|a| a.stage_index).unwrap_or(0);
                }
            }
            KeyCode::Char('/') => {
                self.list_search_mode = true;
                self.list_search_query.clear();
                self.selected = 0;
                self.update_display_indices();
            }
            KeyCode::Char('d') => {
                let info = self
                    .selected_agent()
                    .map(|a| (a.id.clone(), a.is_run_state));
                if let Some((id, is_run_state)) = info {
                    if is_run_state {
                        self.confirm_delete = true;
                        self.add_log(format!(
                            "Delete run '{}'? This kills the process and is PERMANENT. (y/n)",
                            id
                        ));
                    } else {
                        self.add_log(
                            "Only background runs can be deleted from the dashboard".to_string(),
                        );
                    }
                }
            }
            KeyCode::Char('c') => {
                self.handle_cancel_from_list();
            }
            KeyCode::Char('k') => {
                self.handle_kill_from_list();
            }
            KeyCode::Char('?') => {
                self.show_help = true;
            }
            _ => {}
        }
    }

    fn handle_cancel_from_list(&mut self) {
        if let Some(agent) = self.selected_agent() {
            if matches!(
                agent.status,
                AgentDisplayStatus::Active | AgentDisplayStatus::Waiting
            ) {
                let agent_id = agent.id.clone();
                if agent.is_run_state {
                    #[cfg(unix)]
                    if agent.pid > 0 {
                        unsafe {
                            libc::kill(agent.pid as libc::pid_t, libc::SIGTERM);
                        }
                    }
                    kill_write_cancelled(&agent_id);
                    if matches!(agent.status, AgentDisplayStatus::Waiting) {
                        interaction::clear_interaction(&agent_id);
                    }
                    if let Some(a) = self.selected_agent_mut() {
                        a.status = AgentDisplayStatus::Cancelled;
                        a.waiting_prompt = None;
                        a.pending_request = None;
                    }
                } else {
                    let _ = self.cmd_tx.send(EngineCommand::CancelAgent {
                        agent_id: agent_id.clone(),
                    });
                }
                self.add_log(format!("{}: Cancel requested", agent_id));
            }
        }
    }

    fn handle_kill_from_list(&mut self) {
        if let Some(agent) = self.selected_agent() {
            if matches!(
                agent.status,
                AgentDisplayStatus::Active | AgentDisplayStatus::Waiting
            ) {
                let agent_id = agent.id.clone();
                if agent.is_run_state {
                    #[cfg(unix)]
                    if agent.pid > 0 {
                        unsafe {
                            libc::kill(agent.pid as libc::pid_t, libc::SIGTERM);
                        }
                    }
                    kill_write_cancelled(&agent_id);
                    if matches!(agent.status, AgentDisplayStatus::Waiting) {
                        interaction::clear_interaction(&agent_id);
                    }
                } else {
                    let _ = self.cmd_tx.send(EngineCommand::CancelAgent {
                        agent_id: agent_id.clone(),
                    });
                }
                if let Some(a) = self.selected_agent_mut() {
                    a.status = AgentDisplayStatus::Cancelled;
                    a.waiting_prompt = None;
                    a.pending_request = None;
                }
                self.add_log(format!("{}: Killed", agent_id));
            }
        }
    }

    pub(super) fn submit_input(&mut self) {
        use interaction::{ApprovalScope, InteractionKind, InteractionResponse};

        let (agent_id, is_run_state, req) = match self.selected_agent() {
            Some(a) => (a.id.clone(), a.is_run_state, a.pending_request.clone()),
            None => return,
        };

        let (resp, display) = match &req {
            Some(r) => match r.kind {
                InteractionKind::FreeText => {
                    let raw = self.input_textarea.lines().join("\n").trim().to_string();
                    let input = if raw == "/quit" || raw == "/exit" {
                        String::new()
                    } else {
                        raw
                    };
                    let d = if input.is_empty() {
                        "(end)".to_string()
                    } else {
                        truncate(&input, 40)
                    };
                    (InteractionResponse::text(&r.id, &input), d)
                }
                InteractionKind::MultipleChoice => {
                    let idx = self.choice_selected;
                    let label = r
                        .options
                        .get(idx)
                        .cloned()
                        .unwrap_or_else(|| idx.to_string());
                    let d = truncate(&label, 40);
                    (InteractionResponse::choice(&r.id, idx), d)
                }
                InteractionKind::ToolApproval => {
                    let idx = self.choice_selected;
                    let label = r
                        .options
                        .get(idx)
                        .cloned()
                        .unwrap_or_else(|| idx.to_string());
                    let d = truncate(&label, 40);
                    let (approved, scope) = match idx {
                        0 => (true, ApprovalScope::Once),
                        1 => (true, ApprovalScope::Session),
                        _ => (false, ApprovalScope::Once),
                    };
                    (InteractionResponse::approval(&r.id, approved, scope), d)
                }
                InteractionKind::Confirm => {
                    let approved = self.choice_selected == 0;
                    let label = if approved { "Yes" } else { "No" };
                    (
                        InteractionResponse::approval(&r.id, approved, ApprovalScope::Once),
                        label.to_string(),
                    )
                }
            },
            None => {
                let raw = self.input_textarea.lines().join("\n").trim().to_string();
                let input = if raw == "/quit" || raw == "/exit" {
                    String::new()
                } else {
                    raw
                };
                let d = if input.is_empty() {
                    "(end)".to_string()
                } else {
                    truncate(&input, 40)
                };
                (
                    InteractionResponse {
                        request_id: String::new(),
                        value: Some(input),
                        choice_index: None,
                        approved: None,
                        scope: None,
                    },
                    d,
                )
            }
        };

        self.input_mode = false;
        self.input_textarea = tui_textarea::TextArea::default();
        self.choice_selected = 0;

        let answered_id = resp.request_id.clone();
        if let Some(a) = self.selected_agent_mut() {
            a.last_answered_request_id = if answered_id.is_empty() {
                None
            } else {
                Some(answered_id)
            };
            a.waiting_prompt = None;
            a.pending_request = None;
            a.status = AgentDisplayStatus::Active;
        }

        if is_run_state {
            match interaction::write_response(&agent_id, &resp) {
                Ok(()) => self.add_log(format!("Sent: {}", display)),
                Err(e) => self.add_log(format!("Failed to send response: {}", e)),
            }
        } else {
            let input_text = resp
                .value
                .or_else(|| resp.choice_index.map(|i| i.to_string()))
                .unwrap_or_default();
            let _ = self.cmd_tx.send(EngineCommand::SendInput {
                agent_id: agent_id.clone(),
                input: input_text,
            });
            if req.is_none() {
                self.add_log(format!("💬 User: \"{}\"", display));
            } else {
                self.add_log(format!("Sent: {}", display));
            }
        }
    }

    pub(super) fn process_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                AgentEvent::StageChanged { agent_id, stage } => {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                        agent.stage = stage.clone();
                    }
                    self.add_log(format!("{}: Stage -> {}", agent_id, stage));
                }
                AgentEvent::StatusChanged { agent_id, status } => {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                        agent.status = status;
                    }
                }
                AgentEvent::NeedsInput { agent_id, prompt } => {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                        agent.status = AgentDisplayStatus::Waiting;
                        agent.waiting_prompt = Some(prompt.clone());
                    }
                    self.add_log(format!("{}: Waiting for input", agent_id));
                }
                AgentEvent::ToolCalled {
                    agent_id,
                    tool,
                    args,
                } => {
                    self.add_log(format!(
                        "{}: Tool {}({})",
                        agent_id,
                        tool,
                        truncate(&args, 40)
                    ));
                }
                AgentEvent::InferenceComplete {
                    agent_id,
                    content,
                    tokens_used,
                    tokens_prompt,
                } => {
                    if let Some(_agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                        _agent.iteration += 1;
                    }
                    self.add_log(format!(
                        "{}: Inference done ({}tok in, {}tok out) {}",
                        agent_id,
                        tokens_prompt,
                        tokens_used,
                        truncate(&content, 60)
                    ));
                }
                AgentEvent::Error { agent_id, error } => {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                        agent.status = AgentDisplayStatus::Error(error.clone());
                    }
                    self.add_log(format!("{}: ERROR: {}", agent_id, error));
                }
                AgentEvent::Log(msg) => {
                    self.add_log(msg);
                }
                AgentEvent::AgentDone { agent_id } => {
                    if let Some(agent) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                        if !matches!(
                            agent.status,
                            AgentDisplayStatus::Error(_) | AgentDisplayStatus::Cancelled
                        ) {
                            agent.status = AgentDisplayStatus::Complete;
                        }
                    }
                    self.add_log(format!("{}: Done", agent_id));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use tokio::sync::mpsc;

    fn make_test_dashboard() -> Dashboard {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        Dashboard::new(cmd_tx)
    }

    fn make_test_agent(id: &str, status: AgentDisplayStatus) -> DashboardAgent {
        DashboardAgent {
            id: id.to_string(),
            blueprint_name: "test-agent".to_string(),
            agent_path: "/path".to_string(),
            stage: "main".to_string(),
            stage_index: 0,
            num_stages: 2,
            status,
            tokens_in: 0,
            tokens_out: 0,
            cached_tokens: 0,
            context_tokens: (0, 0),
            iteration: 0,
            waiting_prompt: None,
            pending_request: None,
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
            entity: bevy_ecs::prelude::Entity::from_raw(0),
            is_run_state: true,
            pid: 0,
            workdir: "/tmp".to_string(),
            task: "test".to_string(),
            title: None,
            model: None,
            parent_id: None,
            depth: 0,
            started_at: 1000,
            active_until: None,
            waiting_secs: 0,
            graph_info: None,
            accepts_messages: true,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn key_esc_quits_from_main_list() {
        let mut dash = make_test_dashboard();
        dash.handle_key(key(KeyCode::Esc));
        assert!(dash.should_quit);
    }

    #[test]
    fn key_esc_clears_filter_first() {
        let mut dash = make_test_dashboard();
        dash.list_search_query = "test".to_string();
        dash.handle_key(key(KeyCode::Esc));
        assert!(!dash.should_quit);
        assert!(dash.list_search_query.is_empty());
    }

    #[test]
    fn key_enter_opens_detail_view() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Enter));
        assert!(dash.detail_view);
    }

    #[test]
    fn key_enter_no_effect_empty_list() {
        let mut dash = make_test_dashboard();
        dash.handle_key(key(KeyCode::Enter));
        assert!(!dash.detail_view);
    }

    #[test]
    fn key_up_down_moves_selection() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.agents
            .push(make_test_agent("run-2", AgentDisplayStatus::Active));
        dash.update_display_indices();
        assert_eq!(dash.selected, 0);
        dash.handle_key(key(KeyCode::Down));
        assert_eq!(dash.selected, 1);
        dash.handle_key(key(KeyCode::Up));
        assert_eq!(dash.selected, 0);
    }

    #[test]
    fn key_up_at_top_stays() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Up));
        assert_eq!(dash.selected, 0);
    }

    #[test]
    fn key_slash_enters_list_search() {
        let mut dash = make_test_dashboard();
        dash.handle_key(key(KeyCode::Char('/')));
        assert!(dash.list_search_mode);
    }

    #[test]
    fn key_question_shows_help() {
        let mut dash = make_test_dashboard();
        dash.handle_key(key(KeyCode::Char('?')));
        assert!(dash.show_help);
    }

    #[test]
    fn help_overlay_dismissed_by_any_key() {
        let mut dash = make_test_dashboard();
        dash.show_help = true;
        dash.handle_key(key(KeyCode::Char('x')));
        assert!(!dash.show_help);
    }

    #[test]
    fn detail_view_esc_returns_to_list() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.handle_key(key(KeyCode::Esc));
        assert!(!dash.detail_view);
    }

    #[test]
    fn detail_view_esc_clears_search_first() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.search_query = "test".to_string();
        dash.handle_key(key(KeyCode::Esc));
        assert!(dash.detail_view); // still in detail view
        assert!(dash.search_query.is_empty());
    }

    #[test]
    fn detail_view_left_right_switches_stage() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.selected_stage = 0;
        dash.handle_key(key(KeyCode::Right));
        assert_eq!(dash.selected_stage, 1);
        dash.handle_key(key(KeyCode::Left));
        assert_eq!(dash.selected_stage, 0);
    }

    #[test]
    fn detail_view_content_mode_toggle() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.handle_key(key(KeyCode::Char('l')));
        assert_eq!(dash.stage_content_mode, StageContentMode::Logs);
        dash.handle_key(key(KeyCode::Char('o')));
        assert_eq!(dash.stage_content_mode, StageContentMode::Output);
        dash.handle_key(key(KeyCode::Char('c')));
        assert_eq!(dash.stage_content_mode, StageContentMode::Context);
    }

    #[test]
    fn detail_view_search_mode() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.handle_key(key(KeyCode::Char('/')));
        assert!(dash.search_mode);
        dash.handle_key(key(KeyCode::Char('t')));
        dash.handle_key(key(KeyCode::Char('e')));
        assert_eq!(dash.search_query, "te");
        dash.handle_key(key(KeyCode::Backspace));
        assert_eq!(dash.search_query, "t");
        dash.handle_key(key(KeyCode::Enter));
        assert!(!dash.search_mode);
        assert_eq!(dash.search_query, "t");
    }

    #[test]
    fn detail_view_search_esc_clears() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.search_mode = true;
        dash.search_query = "test".to_string();
        dash.handle_key(key(KeyCode::Esc));
        assert!(!dash.search_mode);
        assert!(dash.search_query.is_empty());
    }

    #[test]
    fn detail_view_scroll() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.handle_key(key(KeyCode::Up));
        assert_eq!(dash.detail_scroll, 1);
        dash.handle_key(key(KeyCode::Down));
        assert_eq!(dash.detail_scroll, 0);
    }

    #[test]
    fn detail_view_page_scroll() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.handle_key(key(KeyCode::PageUp));
        assert_eq!(dash.detail_scroll, 10);
        dash.handle_key(key(KeyCode::PageDown));
        assert_eq!(dash.detail_scroll, 0);
    }

    #[test]
    fn detail_view_begin_end() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.handle_key(key(KeyCode::Char('b')));
        assert_eq!(dash.detail_scroll, usize::MAX);
        dash.handle_key(key(KeyCode::Char('e')));
        assert_eq!(dash.detail_scroll, 0);
    }

    #[test]
    fn number_keys_jump_to_stage() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.num_stages = 5;
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.handle_key(key(KeyCode::Char('3')));
        assert_eq!(dash.selected_stage, 2);
    }

    #[test]
    fn confirm_delete_y_confirms() {
        let mut dash = make_test_dashboard();
        dash.confirm_delete = true;
        // No agent to actually delete, but the flag should clear
        dash.handle_key(key(KeyCode::Char('y')));
        assert!(!dash.confirm_delete);
    }

    #[test]
    fn confirm_delete_any_key_cancels() {
        let mut dash = make_test_dashboard();
        dash.confirm_delete = true;
        dash.handle_key(key(KeyCode::Char('x')));
        assert!(!dash.confirm_delete);
    }

    #[test]
    fn list_search_mode_typing() {
        let mut dash = make_test_dashboard();
        dash.list_search_mode = true;
        dash.handle_key(key(KeyCode::Char('a')));
        assert_eq!(dash.list_search_query, "a");
        dash.handle_key(key(KeyCode::Char('b')));
        assert_eq!(dash.list_search_query, "ab");
        dash.handle_key(key(KeyCode::Backspace));
        assert_eq!(dash.list_search_query, "a");
    }

    #[test]
    fn list_search_mode_esc_clears() {
        let mut dash = make_test_dashboard();
        dash.list_search_mode = true;
        dash.list_search_query = "test".to_string();
        dash.handle_key(key(KeyCode::Esc));
        assert!(!dash.list_search_mode);
        assert!(dash.list_search_query.is_empty());
    }

    #[test]
    fn list_search_mode_enter_confirms() {
        let mut dash = make_test_dashboard();
        dash.list_search_mode = true;
        dash.list_search_query = "test".to_string();
        dash.handle_key(key(KeyCode::Enter));
        assert!(!dash.list_search_mode);
        assert_eq!(dash.list_search_query, "test"); // preserved
    }
}
