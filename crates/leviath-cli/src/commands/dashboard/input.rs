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

    /// Seed the input textarea when entering input mode. For an `EditText`
    /// pending request the buffer is pre-filled with the request's `body` (the
    /// current document) so the user edits it in place; otherwise it starts empty.
    fn seed_input_textarea(&mut self) {
        use interaction::InteractionKind;
        let seed = self
            .selected_agent()
            .and_then(|a| a.pending_request.as_ref())
            .filter(|r| r.kind == InteractionKind::EditText)
            .and_then(|r| r.body.clone());
        self.input_textarea = match seed {
            Some(body) => {
                tui_textarea::TextArea::new(body.lines().map(|s| s.to_string()).collect())
            }
            None => tui_textarea::TextArea::default(),
        };
    }

    /// Whether the pending request carries a scrollable review document.
    /// `EditText` also uses `body`, but for editing (not scrolling), so it is
    /// explicitly excluded here.
    fn has_scrollable_review(&self) -> bool {
        use interaction::InteractionKind;
        self.selected_agent()
            .and_then(|a| a.pending_request.as_ref())
            .filter(|r| r.kind != InteractionKind::EditText)
            .and_then(|r| r.body.as_deref())
            .map(|b| !b.is_empty())
            .unwrap_or(false)
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
            Some(InteractionKind::FreeText) | Some(InteractionKind::EditText) | None => {
                match key_code {
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
                }
            }
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
                // Respond to a pending interaction, or send a mid-run message to
                // any active agent that accepts them — same key, same input area.
                if self.selected_stage_can_respond() || self.selected_agent_accepts_messages() {
                    self.input_mode = true;
                    self.choice_selected = 0;
                    self.seed_input_textarea();
                }
            }
            KeyCode::Up => {
                // When a review body is present, Up scrolls the review document
                let has_review = self.has_scrollable_review();
                if has_review {
                    self.review_scroll = self.review_scroll.saturating_add(1);
                } else {
                    self.detail_scroll = self.detail_scroll.saturating_add(1);
                }
            }
            KeyCode::Down => {
                let has_review = self.has_scrollable_review();
                if has_review {
                    self.review_scroll = self.review_scroll.saturating_sub(1);
                } else {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                }
            }
            KeyCode::PageUp => {
                let has_review = self.has_scrollable_review();
                if has_review {
                    self.review_scroll = self.review_scroll.saturating_add(10);
                } else {
                    self.detail_scroll = self.detail_scroll.saturating_add(10);
                }
            }
            KeyCode::PageDown => {
                let has_review = self.has_scrollable_review();
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
        self.handle_yank_with_fn(self.yank_fn);
    }

    fn handle_yank_with_fn(&mut self, yank_fn: fn(&str) -> bool) {
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
                } else if yank_fn(&content) {
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
                    leviath_sys::terminate(_pid);
                    kill_write_cancelled(&agent_id);
                    if was_waiting {
                        interaction::clear_interaction(&agent_id);
                    }
                } else {
                    let _ = self.cmd_tx.send(EngineCommand::CancelAgent {
                        agent_id: agent_id.clone(),
                    });
                }
                // The index came from `display_indices`/`agents` just above,
                // via the `self.selected_agent()` lookup that got us into
                // this branch, and nothing in between (the OS kill signal,
                // `kill_write_cancelled`, `clear_interaction`, or the
                // `cmd_tx.send`) mutates either collection or `self.selected`
                // -- so it's always still valid. An `if let` guard here would
                // add an "index went stale" branch that can never actually be
                // exercised.
                let idx = self
                    .selected_agent_raw_idx()
                    .expect("selected_agent() returned Some above");
                let a = self.agents.get_mut(idx).expect(
                    "index snapshotted from the still-unchanged display_indices/agents above",
                );
                a.status = AgentDisplayStatus::Cancelled;
                a.waiting_prompt = None;
                a.pending_request = None;
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
                    leviath_sys::terminate(agent.pid);
                    kill_write_cancelled(&agent_id);
                    if matches!(agent.status, AgentDisplayStatus::Waiting) {
                        interaction::clear_interaction(&agent_id);
                    }
                    // See the comment in `handle_kill_from_detail` -- the
                    // index is still valid because nothing since the
                    // `self.selected_agent()` lookup above touches
                    // `display_indices`/`agents`/`selected`.
                    let idx = self
                        .selected_agent_raw_idx()
                        .expect("selected_agent() returned Some above");
                    let a = self.agents.get_mut(idx).expect(
                        "index snapshotted from the still-unchanged display_indices/agents above",
                    );
                    a.status = AgentDisplayStatus::Cancelled;
                    a.waiting_prompt = None;
                    a.pending_request = None;
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
                    leviath_sys::terminate(agent.pid);
                    kill_write_cancelled(&agent_id);
                    if matches!(agent.status, AgentDisplayStatus::Waiting) {
                        interaction::clear_interaction(&agent_id);
                    }
                } else {
                    let _ = self.cmd_tx.send(EngineCommand::CancelAgent {
                        agent_id: agent_id.clone(),
                    });
                }
                // See the comment in `handle_kill_from_detail` -- the index
                // is still valid because nothing since the
                // `self.selected_agent()` lookup above touches
                // `display_indices`/`agents`/`selected`.
                let idx = self
                    .selected_agent_raw_idx()
                    .expect("selected_agent() returned Some above");
                let a = self.agents.get_mut(idx).expect(
                    "index snapshotted from the still-unchanged display_indices/agents above",
                );
                a.status = AgentDisplayStatus::Cancelled;
                a.waiting_prompt = None;
                a.pending_request = None;
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
                InteractionKind::EditText => {
                    // Preserve indentation / internal newlines — only trim the
                    // display label, not the submitted content.
                    let input = self.input_textarea.lines().join("\n");
                    let d = if input.trim().is_empty() {
                        "(no changes)".to_string()
                    } else {
                        truncate(input.trim(), 40)
                    };
                    (InteractionResponse::text(&r.id, &input), d)
                }
                InteractionKind::MultipleChoice => {
                    let idx = self.choice_selected;
                    let label = r.options.get(idx).cloned().unwrap_or(idx.to_string());
                    let d = truncate(&label, 40);
                    (InteractionResponse::choice(&r.id, idx), d)
                }
                InteractionKind::ToolApproval => {
                    let idx = self.choice_selected;
                    let label = r.options.get(idx).cloned().unwrap_or(idx.to_string());
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
        // The index is still valid because nothing since the
        // `self.selected_agent()` lookup at the top of this function (which
        // is where `req`/`agent_id`/`is_run_state` came from) touches
        // `display_indices`/`agents`/`selected` -- an `if let` guard here
        // would add an "index went stale" branch that can never actually be
        // exercised.
        let idx = self
            .selected_agent_raw_idx()
            .expect("selected_agent() returned Some above");
        let a = self
            .agents
            .get_mut(idx)
            .expect("index snapshotted from the still-unchanged display_indices/agents above");
        a.last_answered_request_id = if answered_id.is_empty() {
            None
        } else {
            Some(answered_id)
        };
        a.waiting_prompt = None;
        a.pending_request = None;
        a.status = AgentDisplayStatus::Active;

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

    use crate::commands::dashboard::test_support::make_test_dashboard;

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
            taint_summary: vec![],
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    // ── Status assertion helpers (both branches covered by #[should_panic] companions) ──

    fn assert_cancelled(status: &AgentDisplayStatus) {
        if !matches!(status, AgentDisplayStatus::Cancelled) {
            panic!("expected Cancelled, got {:?}", status);
        }
    }

    fn assert_complete(status: &AgentDisplayStatus) {
        if !matches!(status, AgentDisplayStatus::Complete) {
            panic!("expected Complete, got {:?}", status);
        }
    }

    #[test]
    #[should_panic(expected = "expected Cancelled")]
    fn assert_cancelled_helper_panics_on_wrong_status() {
        assert_cancelled(&AgentDisplayStatus::Active);
    }

    #[test]
    #[should_panic(expected = "expected Complete")]
    fn assert_complete_helper_panics_on_wrong_status() {
        assert_complete(&AgentDisplayStatus::Active);
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

    #[test]
    fn list_search_mode_unhandled_key_is_noop() {
        let mut dash = make_test_dashboard();
        dash.list_search_mode = true;
        dash.list_search_query = "test".to_string();
        dash.handle_key(key(KeyCode::Left));
        assert!(dash.list_search_mode);
        assert_eq!(dash.list_search_query, "test");
    }

    #[test]
    fn detail_view_search_mode_unhandled_key_is_noop() {
        let mut dash = make_test_dashboard();
        let agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.search_mode = true;
        dash.search_query = "test".to_string();
        dash.handle_key(key(KeyCode::Left));
        assert!(dash.search_mode);
        assert_eq!(dash.search_query, "test");
    }

    // ─── handle_input_mode_key for choice navigation ──────────────────────

    #[test]
    fn input_mode_choice_up_down() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(crate::interaction::InteractionRequest::multiple_choice(
            "mc1",
            "Pick one",
            vec!["A".into(), "B".into(), "C".into()],
            "main",
        ));
        agent.waiting_prompt = Some("Pick one".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;
        dash.choice_selected = 0;

        // Down
        dash.handle_key(key(KeyCode::Down));
        assert_eq!(dash.choice_selected, 1);
        dash.handle_key(key(KeyCode::Down));
        assert_eq!(dash.choice_selected, 2);
        // Down at bottom stays
        dash.handle_key(key(KeyCode::Down));
        assert_eq!(dash.choice_selected, 2);

        // Up
        dash.handle_key(key(KeyCode::Up));
        assert_eq!(dash.choice_selected, 1);
        dash.handle_key(key(KeyCode::Up));
        assert_eq!(dash.choice_selected, 0);
        // Up at top stays
        dash.handle_key(key(KeyCode::Up));
        assert_eq!(dash.choice_selected, 0);
    }

    #[test]
    fn input_mode_esc_exits() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(crate::interaction::InteractionRequest::free_text(
            "ft1", "prompt", "main", true,
        ));
        agent.waiting_prompt = Some("prompt".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;

        dash.handle_key(key(KeyCode::Esc));
        assert!(!dash.input_mode);
    }

    #[test]
    fn input_mode_free_text_typing_appends_to_textarea() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(crate::interaction::InteractionRequest::free_text(
            "ft1", "prompt", "main", true,
        ));
        agent.waiting_prompt = Some("prompt".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;

        dash.handle_key(key(KeyCode::Char('h')));
        dash.handle_key(key(KeyCode::Char('i')));
        assert_eq!(dash.input_textarea.lines(), vec!["hi".to_string()]);
    }

    #[test]
    fn input_mode_edit_text_typing_appends_to_textarea() {
        // EditText routes through the text-editing key arm just like FreeText.
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(crate::interaction::InteractionRequest::edit_text(
            "et1", "Edit", "main", "seed",
        ));
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;

        dash.handle_key(key(KeyCode::Char('!')));
        assert_eq!(dash.input_textarea.lines(), vec!["!".to_string()]);
    }

    #[test]
    fn seed_input_textarea_prefills_edit_text_body() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(crate::interaction::InteractionRequest::edit_text(
            "et1",
            "Edit",
            "main",
            "line A\nline B",
        ));
        dash.agents.push(agent);
        dash.update_display_indices();

        dash.seed_input_textarea();
        assert_eq!(
            dash.input_textarea.lines(),
            vec!["line A".to_string(), "line B".to_string()]
        );
    }

    #[test]
    fn seed_input_textarea_empty_for_free_text() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(crate::interaction::InteractionRequest::free_text(
            "ft1", "Q?", "main", true,
        ));
        dash.agents.push(agent);
        dash.update_display_indices();

        dash.input_textarea.insert_str("stale");
        dash.seed_input_textarea();
        // FreeText is not seeded from body → cleared to empty.
        assert_eq!(dash.input_textarea.lines(), vec!["".to_string()]);
    }

    #[test]
    fn submit_input_edit_text_preserves_newlines() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.is_run_state = false; // cmd_tx path — capture the SendInput value
        agent.pending_request = Some(crate::interaction::InteractionRequest::edit_text(
            "et1", "Edit", "main", "old",
        ));
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;

        dash.input_textarea =
            tui_textarea::TextArea::new(vec!["  indented".to_string(), "second line".to_string()]);
        dash.submit_input();

        assert!(!dash.input_mode);
        assert!(dash.agents[0].pending_request.is_none());
        // The edited text reached the engine with indentation + newline intact.
        let cmd = cmd_rx.try_recv().expect("a SendInput command was queued");
        // The edited text reached the engine with indentation + newline intact.
        assert_eq!(
            cmd,
            EngineCommand::SendInput {
                agent_id: "run-1".to_string(),
                input: "  indented\nsecond line".to_string(),
            }
        );
    }

    #[test]
    fn submit_input_edit_text_empty_reports_no_changes() {
        // Covers the empty-edit display branch of the EditText submit arm.
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.is_run_state = false;
        agent.pending_request = Some(crate::interaction::InteractionRequest::edit_text(
            "et1", "Edit", "main", "old",
        ));
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;

        // Whitespace-only edit → trimmed empty → "(no changes)" display path.
        dash.input_textarea = tui_textarea::TextArea::new(vec!["   ".to_string()]);
        dash.submit_input();

        assert!(!dash.input_mode);
        let cmd = cmd_rx.try_recv().expect("a SendInput command was queued");
        assert_eq!(
            cmd,
            EngineCommand::SendInput {
                agent_id: "run-1".to_string(),
                input: "   ".to_string(),
            }
        );
    }

    #[test]
    fn input_mode_no_pending_request_typing_appends_to_textarea() {
        // kind resolves to None when there's no pending_request at all —
        // exercises the `Some(FreeText) | None` arm's None side.
        let mut dash = make_test_dashboard();
        let agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;

        dash.handle_key(key(KeyCode::Char('x')));
        assert_eq!(dash.input_textarea.lines(), vec!["x".to_string()]);
    }

    #[test]
    fn input_mode_esc_on_choice_resets() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(crate::interaction::InteractionRequest::multiple_choice(
            "mc1",
            "Pick",
            vec!["A".into(), "B".into()],
            "main",
        ));
        agent.waiting_prompt = Some("Pick".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;
        dash.choice_selected = 1;

        dash.handle_key(key(KeyCode::Esc));
        assert!(!dash.input_mode);
        assert_eq!(dash.choice_selected, 0);
    }

    // ─── handle_cancel_from_list ──────────────────────────────────────────

    #[test]
    fn cancel_from_list_active_agent() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Char('c')));
        // Agent should be cancelled (is_run_state = true, pid = 0)
        assert_cancelled(&dash.agents[0].status);
    }

    #[test]
    fn cancel_from_list_complete_agent_no_op() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Complete));
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Char('c')));
        // Should remain Complete
        assert_complete(&dash.agents[0].status);
    }

    #[test]
    fn cancel_from_list_waiting_agent_clears_interaction() {
        crate::runstate::with_isolated_runs_dir(
            "cancel_from_list_waiting_agent_clears_interaction",
            |_d| {
                let run_id = "test-cancel-list-waiting-clears";
                std::fs::create_dir_all(crate::runstate::run_dir(run_id)).unwrap();
                let req =
                    crate::interaction::InteractionRequest::free_text("q1", "?", "main", true);
                let _ = crate::interaction::write_request(run_id, &req);

                let mut dash = make_test_dashboard();
                let mut agent = make_test_agent(run_id, AgentDisplayStatus::Waiting);
                agent.waiting_prompt = Some("?".to_string());
                dash.agents.push(agent);
                dash.update_display_indices();
                dash.handle_key(key(KeyCode::Char('c')));

                assert_cancelled(&dash.agents[0].status);
                assert!(crate::interaction::read_request(run_id).is_none());

                let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
            },
        );
    }

    #[test]
    fn cancel_from_list_with_pid_sends_signal() {
        // See kill_from_detail_with_pid_sends_signal for why this PID value
        // is safe: implausibly large, guaranteed ESRCH no-op.
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.pid = 2_000_000_000;
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Char('c')));
        assert_cancelled(&dash.agents[0].status);
    }

    // ─── handle_kill_from_list ─────────────────────────────────────────────

    #[test]
    fn kill_from_list_active_agent() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Char('k')));
        assert_cancelled(&dash.agents[0].status);
    }

    #[test]
    fn kill_from_list_waiting_agent() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.waiting_prompt = Some("prompt".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Char('k')));
        assert_cancelled(&dash.agents[0].status);
        assert!(dash.agents[0].waiting_prompt.is_none());
    }

    #[test]
    fn kill_from_list_complete_agent_no_op() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Complete));
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Char('k')));
        assert_complete(&dash.agents[0].status);
    }

    #[test]
    fn kill_from_list_with_pid_sends_signal() {
        // See kill_from_detail_with_pid_sends_signal for why this PID value
        // is safe: implausibly large, guaranteed ESRCH no-op.
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.pid = 2_000_000_000;
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Char('k')));
        assert_cancelled(&dash.agents[0].status);
    }

    #[test]
    fn main_list_d_no_agent_selected_is_noop() {
        let mut dash = make_test_dashboard();
        dash.handle_key(key(KeyCode::Char('d')));
        assert!(!dash.confirm_delete);
        assert!(dash.agents.is_empty());
    }

    #[test]
    fn cancel_from_list_no_agent_selected_is_noop() {
        let mut dash = make_test_dashboard();
        // No agents at all — selected_agent() is None.
        dash.handle_key(key(KeyCode::Char('c')));
        assert!(dash.agents.is_empty());
    }

    #[test]
    fn kill_from_list_no_agent_selected_is_noop() {
        let mut dash = make_test_dashboard();
        dash.handle_key(key(KeyCode::Char('k')));
        assert!(dash.agents.is_empty());
    }

    #[test]
    fn kill_from_detail_no_agent_selected_is_noop() {
        let mut dash = make_test_dashboard();
        dash.detail_view = true;
        dash.handle_key(key(KeyCode::Char('k')));
        assert!(dash.agents.is_empty());
    }

    #[test]
    fn main_list_unhandled_key_is_noop() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        let selected_before = dash.selected;
        dash.handle_key(key(KeyCode::Char('z')));
        assert_eq!(dash.selected, selected_before);
    }

    #[test]
    fn submit_input_no_agent_selected_returns_early() {
        let mut dash = make_test_dashboard();
        dash.input_mode = true;
        // No agents at all — selected_agent() is None.
        dash.submit_input();
        assert!(dash.agents.is_empty());
    }

    // ─── handle_kill_from_detail ──────────────────────────────────────────

    #[test]
    fn kill_from_detail_active_agent() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.handle_key(key(KeyCode::Char('k')));
        assert_cancelled(&dash.agents[0].status);
    }

    #[test]
    fn kill_from_detail_complete_agent_no_op() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Complete));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.handle_key(key(KeyCode::Char('k')));
        // Should remain complete
        assert_complete(&dash.agents[0].status);
    }

    #[test]
    fn kill_from_detail_with_pid_sends_signal() {
        // Exercise the `pid > 0` unix-kill branch. Use an implausibly large
        // PID (near i32::MAX, far beyond any real PID_MAX) so `libc::kill`
        // is guaranteed to be a harmless no-op (ESRCH) rather than risking
        // sending SIGTERM to a real, unrelated process.
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.pid = 2_000_000_000;
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.handle_key(key(KeyCode::Char('k')));
        assert_cancelled(&dash.agents[0].status);
    }

    #[test]
    fn kill_from_detail_waiting_agent_clears_interaction() {
        crate::runstate::with_isolated_runs_dir(
            "kill_from_detail_waiting_agent_clears_interaction",
            |_d| {
                let run_id = "test-kill-waiting-clears";
                std::fs::create_dir_all(crate::runstate::run_dir(run_id)).unwrap();
                let req =
                    crate::interaction::InteractionRequest::free_text("q1", "?", "main", true);
                let _ = crate::interaction::write_request(run_id, &req);
                assert!(crate::interaction::read_request(run_id).is_some());

                let mut dash = make_test_dashboard();
                let mut agent = make_test_agent(run_id, AgentDisplayStatus::Waiting);
                agent.waiting_prompt = Some("?".to_string());
                dash.agents.push(agent);
                dash.update_display_indices();
                dash.detail_view = true;
                dash.handle_key(key(KeyCode::Char('k')));

                assert_cancelled(&dash.agents[0].status);
                assert!(crate::interaction::read_request(run_id).is_none());

                let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
            },
        );
    }

    // ─── detail view: search n/N ──────────────────────────────────────────

    #[test]
    fn detail_view_search_n_increments_match() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.search_query = "test".to_string();
        dash.search_match_idx = 0;
        dash.handle_key(key(KeyCode::Char('n')));
        assert_eq!(dash.search_match_idx, 1);
        dash.handle_key(key(KeyCode::Char('n')));
        assert_eq!(dash.search_match_idx, 2);
    }

    #[test]
    fn detail_view_search_shift_n_decrements_match() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.search_query = "test".to_string();
        dash.search_match_idx = 5;
        dash.handle_key(key(KeyCode::Char('N')));
        assert_eq!(dash.search_match_idx, 4);
    }

    #[test]
    fn detail_view_search_n_no_query_no_op() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.search_query.clear();
        dash.search_match_idx = 0;
        dash.handle_key(key(KeyCode::Char('n')));
        assert_eq!(dash.search_match_idx, 0);
    }

    // ─── detail view: unified i key (respond + mid-run message) ───────────

    #[test]
    fn detail_view_i_enters_input_when_can_respond() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.waiting_prompt = Some("prompt".to_string());
        agent.pending_request = Some(crate::interaction::InteractionRequest::free_text(
            "ft1", "prompt", "main", true,
        ));
        agent.stage_index = 0;
        agent.stages = vec![crate::runstate::StageRecord::new("main".to_string(), 0)];
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.selected_stage = 0;

        dash.handle_key(key(KeyCode::Char('i')));
        assert!(dash.input_mode);
    }

    #[test]
    fn detail_view_i_enters_input_for_active_accepting_agent() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.accepts_messages = true;
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;

        dash.handle_key(key(KeyCode::Char('i')));
        assert!(dash.input_mode);
    }

    #[test]
    fn detail_view_i_no_effect_when_not_accepting_and_cannot_respond() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.accepts_messages = false;
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;

        dash.handle_key(key(KeyCode::Char('i')));
        assert!(!dash.input_mode);
    }

    #[test]
    fn detail_view_m_key_no_longer_bound() {
        // The message keybinding is 'i', not 'm'. Pressing 'm' must not enter
        // input mode.
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.accepts_messages = true;
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;

        dash.handle_key(key(KeyCode::Char('m')));
        assert!(!dash.input_mode);
    }

    // ─── detail view: d key for delete ────────────────────────────────────

    #[test]
    fn main_list_d_opens_confirm_delete() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Char('d')));
        assert!(dash.confirm_delete);
    }

    #[test]
    fn main_list_d_non_run_state_agent_no_confirm() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.is_run_state = false;
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Char('d')));
        assert!(!dash.confirm_delete);
    }

    // ─── detail view: review scroll ───────────────────────────────────────

    #[test]
    fn detail_view_scroll_with_review_body() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(crate::interaction::InteractionRequest::review(
            "rev1",
            "Review",
            "# Long markdown body\n\nSome content here.",
            "main",
        ));
        agent.waiting_prompt = Some("Review".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;

        // Up should scroll review_scroll (not detail_scroll)
        dash.handle_key(key(KeyCode::Up));
        assert_eq!(dash.review_scroll, 1);
        assert_eq!(dash.detail_scroll, 0);

        dash.handle_key(key(KeyCode::Down));
        assert_eq!(dash.review_scroll, 0);

        dash.handle_key(key(KeyCode::PageUp));
        assert_eq!(dash.review_scroll, 10);

        dash.handle_key(key(KeyCode::PageDown));
        assert_eq!(dash.review_scroll, 0);
    }

    // ─── handle_input_mode_key for ToolApproval ──────────────────────────

    #[test]
    fn input_mode_tool_approval_up_down() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(crate::interaction::InteractionRequest::tool_approval(
            "ta1",
            "bash",
            serde_json::json!({"cmd": "ls"}),
            "main",
        ));
        agent.waiting_prompt = Some("Allow tool?".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;
        dash.choice_selected = 0;

        // Down through 3 options
        dash.handle_key(key(KeyCode::Down));
        assert_eq!(dash.choice_selected, 1);
        dash.handle_key(key(KeyCode::Down));
        assert_eq!(dash.choice_selected, 2);
        // Can't go past last
        dash.handle_key(key(KeyCode::Down));
        assert_eq!(dash.choice_selected, 2);

        // Up back
        dash.handle_key(key(KeyCode::Up));
        assert_eq!(dash.choice_selected, 1);
    }

    #[test]
    fn input_mode_tool_approval_esc_resets() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(crate::interaction::InteractionRequest::tool_approval(
            "ta1",
            "bash",
            serde_json::json!({"cmd": "ls"}),
            "main",
        ));
        agent.waiting_prompt = Some("Allow tool?".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;
        dash.choice_selected = 2;

        dash.handle_key(key(KeyCode::Esc));
        assert!(!dash.input_mode);
        assert_eq!(dash.choice_selected, 0);
    }

    // ─── submit_input for FreeText ───────────────────────────────────────

    #[test]
    fn submit_input_free_text() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.is_run_state = false;
        agent.pending_request = Some(crate::interaction::InteractionRequest::free_text(
            "ft1", "What?", "main", true,
        ));
        agent.waiting_prompt = Some("What?".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;

        // Type something into the textarea
        dash.input_textarea.insert_str("my answer");

        dash.submit_input();

        assert!(!dash.input_mode);
        assert!(dash.agents[0].pending_request.is_none());
        assert!(dash.agents[0].waiting_prompt.is_none());
        assert_eq!(dash.agents[0].status, AgentDisplayStatus::Active);
    }

    // ─── submit_input for FreeText with /quit ─────────────────────────────

    #[test]
    fn submit_input_free_text_quit() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.is_run_state = false; // use cmd_tx path to avoid disk I/O
        agent.pending_request = Some(crate::interaction::InteractionRequest::free_text(
            "ft1", "What?", "main", true,
        ));
        agent.waiting_prompt = Some("What?".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;

        dash.input_textarea.insert_str("/quit");

        dash.submit_input();

        assert!(!dash.input_mode);
        // Agent status should be Active (resumed)
        assert_eq!(dash.agents[0].status, AgentDisplayStatus::Active);
    }

    // ─── submit_input for MultipleChoice ──────────────────────────────────

    #[test]
    fn submit_input_multiple_choice() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.is_run_state = false;
        agent.pending_request = Some(crate::interaction::InteractionRequest::multiple_choice(
            "mc1",
            "Pick",
            vec!["A".into(), "B".into(), "C".into()],
            "main",
        ));
        agent.waiting_prompt = Some("Pick".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;
        dash.choice_selected = 1; // Select "B"

        dash.submit_input();

        assert!(!dash.input_mode);
        assert_eq!(dash.choice_selected, 0); // reset
        assert!(dash.agents[0].pending_request.is_none());
    }

    // ─── submit_input for ToolApproval with different indices ─────────────

    #[test]
    fn submit_input_tool_approval_allow_once() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.is_run_state = false;
        agent.pending_request = Some(crate::interaction::InteractionRequest::tool_approval(
            "ta1",
            "bash",
            serde_json::json!({"cmd": "ls"}),
            "main",
        ));
        agent.waiting_prompt = Some("Allow tool?".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;
        dash.choice_selected = 0; // "Allow once"

        dash.submit_input();
        assert!(!dash.input_mode);
        assert!(dash.agents[0].pending_request.is_none());
    }

    #[test]
    fn submit_input_tool_approval_allow_session() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.is_run_state = false;
        agent.pending_request = Some(crate::interaction::InteractionRequest::tool_approval(
            "ta1",
            "bash",
            serde_json::json!({"cmd": "ls"}),
            "main",
        ));
        agent.waiting_prompt = Some("Allow tool?".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;
        dash.choice_selected = 1; // "Allow for this session"

        dash.submit_input();
        assert!(!dash.input_mode);
        assert!(dash.agents[0].pending_request.is_none());
    }

    #[test]
    fn submit_input_tool_approval_deny() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.is_run_state = false;
        agent.pending_request = Some(crate::interaction::InteractionRequest::tool_approval(
            "ta1",
            "bash",
            serde_json::json!({"cmd": "ls"}),
            "main",
        ));
        agent.waiting_prompt = Some("Allow tool?".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;
        dash.choice_selected = 2; // "Deny"

        dash.submit_input();
        assert!(!dash.input_mode);
        assert!(dash.agents[0].pending_request.is_none());
    }

    // ─── submit_input for Confirm ─────────────────────────────────────────

    #[test]
    fn submit_input_confirm_yes() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.is_run_state = false;
        agent.pending_request = Some(crate::interaction::InteractionRequest::confirm(
            "c1", "Proceed?", "main",
        ));
        agent.waiting_prompt = Some("Proceed?".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;
        dash.choice_selected = 0; // "Yes"

        dash.submit_input();
        assert!(!dash.input_mode);
        assert!(dash.agents[0].pending_request.is_none());
    }

    #[test]
    fn submit_input_confirm_no() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.is_run_state = false;
        agent.pending_request = Some(crate::interaction::InteractionRequest::confirm(
            "c1", "Proceed?", "main",
        ));
        agent.waiting_prompt = Some("Proceed?".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;
        dash.choice_selected = 1; // "No"

        dash.submit_input();
        assert!(!dash.input_mode);
        assert!(dash.agents[0].pending_request.is_none());
    }

    // ─── submit_input for in-process agent (cmd_tx path) ──────────────────

    #[test]
    fn submit_input_in_process_agent() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.is_run_state = false; // in-process
        agent.pending_request = Some(crate::interaction::InteractionRequest::free_text(
            "ft1", "What?", "main", true,
        ));
        agent.waiting_prompt = Some("What?".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;

        dash.input_textarea.insert_str("in-process answer");

        dash.submit_input();

        assert!(!dash.input_mode);
        // Should have sent EngineCommand::SendInput
        let cmd = cmd_rx.try_recv();
        assert!(cmd.is_ok());
    }

    // ─── submit_input with no pending request (mid-run message) ───────────

    #[test]
    fn submit_input_no_pending_request() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.is_run_state = false;
        agent.pending_request = None;
        agent.waiting_prompt = None;
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;

        dash.input_textarea.insert_str("hello mid-run");

        dash.submit_input();

        assert!(!dash.input_mode);
    }

    // ─── submit_input with no pending request /exit ───────────────────────

    #[test]
    fn submit_input_no_pending_request_exit() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.is_run_state = false;
        agent.pending_request = None;
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;

        dash.input_textarea.insert_str("/exit");
        dash.submit_input();

        assert!(!dash.input_mode);
    }

    // ─── process_events for InferenceComplete ─────────────────────────────

    #[test]
    fn process_events_inference_complete_increments_iteration() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        let tx = dash.event_tx.clone();
        tx.send(AgentEvent::InferenceComplete {
            agent_id: "run-1".to_string(),
            content: "analysis complete".to_string(),
            tokens_used: 200,
            tokens_prompt: 100,
        })
        .unwrap();
        dash.process_events();
        assert_eq!(dash.agents[0].iteration, 1);
    }

    // ─── process_events for NeedsInput ────────────────────────────────────

    #[test]
    fn process_events_needs_input_sets_waiting() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        let tx = dash.event_tx.clone();
        tx.send(AgentEvent::NeedsInput {
            agent_id: "run-1".to_string(),
            prompt: "Please provide input".to_string(),
        })
        .unwrap();
        dash.process_events();
        assert_eq!(dash.agents[0].status, AgentDisplayStatus::Waiting);
        assert_eq!(
            dash.agents[0].waiting_prompt.as_deref(),
            Some("Please provide input")
        );
    }

    // ─── process_events AgentDone doesn't override Error/Cancelled ────────

    #[test]
    fn process_events_agent_done_for_unknown_agent_is_noop() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        let tx = dash.event_tx.clone();
        // Event for an agent_id that isn't in dash.agents at all.
        tx.send(AgentEvent::AgentDone {
            agent_id: "nonexistent-run".to_string(),
        })
        .unwrap();
        dash.process_events();
        // The unrelated existing agent must be untouched.
        assert_eq!(dash.agents[0].status, AgentDisplayStatus::Active);
    }

    #[test]
    fn process_events_agent_done_does_not_override_error() {
        let mut dash = make_test_dashboard();
        dash.agents.push(make_test_agent(
            "run-1",
            AgentDisplayStatus::Error("failed".to_string()),
        ));
        dash.update_display_indices();
        let tx = dash.event_tx.clone();
        tx.send(AgentEvent::AgentDone {
            agent_id: "run-1".to_string(),
        })
        .unwrap();
        dash.process_events();
        // Should still be Error("failed"), not Complete.
        assert_eq!(
            dash.agents[0].status,
            AgentDisplayStatus::Error("failed".to_string())
        );
    }

    #[test]
    fn process_events_agent_done_does_not_override_cancelled() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Cancelled));
        dash.update_display_indices();
        let tx = dash.event_tx.clone();
        tx.send(AgentEvent::AgentDone {
            agent_id: "run-1".to_string(),
        })
        .unwrap();
        dash.process_events();
        assert_eq!(dash.agents[0].status, AgentDisplayStatus::Cancelled);
    }

    // ─── handle_kill_from_detail for in-process agent ─────────────────────

    #[test]
    fn kill_from_detail_in_process_agent_sends_command() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.is_run_state = false;
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;

        dash.handle_key(key(KeyCode::Char('k')));

        assert_eq!(dash.agents[0].status, AgentDisplayStatus::Cancelled);

        // Should have sent CancelAgent command
        let cmd = cmd_rx.try_recv();
        assert!(cmd.is_ok());
    }

    // ─── handle_cancel_from_list for in-process agent ─────────────────────

    #[test]
    fn cancel_from_list_in_process_agent_sends_command() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.is_run_state = false;
        dash.agents.push(agent);
        dash.update_display_indices();

        dash.handle_key(key(KeyCode::Char('c')));

        // Should have sent CancelAgent command (no status change for in-process cancel)
        let cmd = cmd_rx.try_recv();
        assert!(cmd.is_ok());
    }

    // ─── handle_kill_from_list for in-process agent ───────────────────────

    #[test]
    fn kill_from_list_in_process_agent_sends_command() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.is_run_state = false;
        dash.agents.push(agent);
        dash.update_display_indices();

        dash.handle_key(key(KeyCode::Char('k')));

        assert_eq!(dash.agents[0].status, AgentDisplayStatus::Cancelled);

        let cmd = cmd_rx.try_recv();
        assert!(cmd.is_ok());
    }

    // ─── handle_yank generates toast for run-state agent ──────────────────

    #[test]
    fn yank_generates_toast_for_empty_content() {
        let mut dash = make_test_dashboard();
        let agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.stage_content_mode = StageContentMode::Output;

        dash.handle_key(key(KeyCode::Char('y')));

        // Should have generated a toast (either "No Output content" or clipboard result)
        assert!(!dash.toasts.is_empty());
    }

    #[test]
    fn yank_logs_mode_empty_content_toast() {
        let mut dash = make_test_dashboard();
        let agent = make_test_agent("run-yank-logs", AgentDisplayStatus::Active);
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.stage_content_mode = StageContentMode::Logs;

        dash.handle_key(key(KeyCode::Char('y')));

        assert!(!dash.toasts.is_empty());
    }

    #[test]
    fn yank_context_mode_empty_content_toast() {
        let mut dash = make_test_dashboard();
        let agent = make_test_agent("run-yank-context", AgentDisplayStatus::Active);
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.stage_content_mode = StageContentMode::Context;

        dash.handle_key(key(KeyCode::Char('y')));

        assert!(!dash.toasts.is_empty());
    }

    #[test]
    fn yank_with_real_content_reports_success() {
        // Uses `handle_yank_with_fn` with an injected always-succeeds clipboard
        // fn for a deterministic toast assertion. (`handle_yank` itself uses
        // the dashboard's injected `yank_fn`, which is a no-op under
        // `make_test_dashboard`; the native-tool-vs-OSC52 branches live in
        // `helpers::yank_to_clipboard_via`'s own tests, and the real OSC52
        // write in `leviath_sys::tty`.)
        crate::runstate::with_isolated_runs_dir("yank_with_real_content_reports_success", |_d| {
            let run_id = "test-yank-real-content";
            crate::runstate::append_stage_output(run_id, 0, "some real output");

            let mut dash = make_test_dashboard();
            let agent = make_test_agent(run_id, AgentDisplayStatus::Active);
            dash.agents.push(agent);
            dash.update_display_indices();
            dash.detail_view = true;
            dash.stage_content_mode = StageContentMode::Output;

            dash.handle_yank_with_fn(|_| true);

            assert!(dash
                .toasts
                .iter()
                .any(|t| t.message.contains("yanked to clipboard")));

            let _ = std::fs::remove_dir_all(crate::runstate::run_dir(run_id));
        });
    }

    #[test]
    fn detail_view_question_mark_shows_help() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.handle_key(key(KeyCode::Char('?')));
        assert!(dash.show_help);
    }

    // ─── d key from main list for non-run-state agent ─────────────────────

    #[test]
    fn main_list_d_for_non_run_state_logs_message() {
        let mut dash = make_test_dashboard();
        dash.log.clear();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.is_run_state = false;
        dash.agents.push(agent);
        dash.update_display_indices();

        dash.handle_key(key(KeyCode::Char('d')));

        assert!(!dash.confirm_delete);
        assert!(dash
            .log
            .iter()
            .any(|e| e.message.contains("Only background runs")));
    }

    // ─── input_mode_key: FreeText Enter submits ───────────────────────────

    #[test]
    fn input_mode_free_text_enter_submits() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(crate::interaction::InteractionRequest::free_text(
            "ft1", "prompt", "main", true,
        ));
        agent.waiting_prompt = Some("prompt".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;

        dash.input_textarea.insert_str("answer");

        // Enter with no modifiers should submit
        dash.handle_key(key(KeyCode::Enter));
        assert!(!dash.input_mode);
    }

    // ─── input_mode_key: MultipleChoice Enter submits ─────────────────────

    #[test]
    fn input_mode_multiple_choice_enter_submits() {
        crate::runstate::with_isolated_runs_dir("input_mode_multiple_choice_enter_submits", |_d| {
            let run_id = format!("test-mc-enter-{}", std::process::id());
            let mut dash = make_test_dashboard();
            let mut agent = make_test_agent(&run_id, AgentDisplayStatus::Waiting);
            agent.pending_request = Some(crate::interaction::InteractionRequest::multiple_choice(
                "mc1",
                "Pick",
                vec!["A".into(), "B".into()],
                "main",
            ));
            agent.waiting_prompt = Some("Pick".to_string());
            dash.agents.push(agent);
            dash.update_display_indices();
            dash.detail_view = true;
            dash.input_mode = true;
            dash.choice_selected = 0;

            // Ensure the run directory exists so write_response succeeds
            let _ = std::fs::create_dir_all(crate::runstate::run_dir(&run_id));

            dash.handle_key(key(KeyCode::Enter));
            assert!(!dash.input_mode);
            assert!(dash.log.iter().any(|e| e.message.contains("A")));

            let _ = std::fs::remove_dir_all(crate::runstate::run_dir(&run_id));
        });
    }

    // ─── Multiple events processed in sequence ────────────────────────────

    #[test]
    fn process_multiple_events_in_sequence() {
        let mut dash = make_test_dashboard();
        dash.log.clear();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.agents
            .push(make_test_agent("run-2", AgentDisplayStatus::Active));
        dash.update_display_indices();
        let tx = dash.event_tx.clone();
        tx.send(AgentEvent::StageChanged {
            agent_id: "run-1".to_string(),
            stage: "code".to_string(),
        })
        .unwrap();
        tx.send(AgentEvent::InferenceComplete {
            agent_id: "run-2".to_string(),
            content: "done".to_string(),
            tokens_used: 50,
            tokens_prompt: 25,
        })
        .unwrap();
        tx.send(AgentEvent::AgentDone {
            agent_id: "run-1".to_string(),
        })
        .unwrap();

        dash.process_events();

        assert_eq!(dash.agents[0].stage, "code");
        assert_eq!(dash.agents[0].status, AgentDisplayStatus::Complete);
        assert_eq!(dash.agents[1].iteration, 1);
    }

    // ─── submit_input last_answered_request_id tracking ───────────────────

    #[test]
    fn submit_input_tracks_last_answered_request_id() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(crate::interaction::InteractionRequest::free_text(
            "req-42", "What?", "main", true,
        ));
        agent.waiting_prompt = Some("What?".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;

        dash.input_textarea.insert_str("answer");
        dash.submit_input();

        assert_eq!(
            dash.agents[0].last_answered_request_id.as_deref(),
            Some("req-42")
        );
    }

    // ─── detail view: Left/Right stage navigation ─────────────────────────

    #[test]
    fn detail_view_left_when_selected_stage_greater_than_zero() {
        // Exercises lines 164-171: the body of `if self.selected_stage > 0`
        // for the Left key in handle_detail_view_key.
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.num_stages = 3;
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.selected_stage = 2;
        dash.detail_scroll = 5;
        dash.review_scroll = 3;
        // search_mode must be FALSE so search mode doesn't intercept the Left key.
        // The handler still clears search_query and search_match_idx.
        dash.search_mode = false;
        dash.search_query = "foo".to_string();
        dash.search_match_idx = 4;

        dash.handle_key(key(KeyCode::Left));

        assert_eq!(dash.selected_stage, 1);
        assert_eq!(dash.detail_scroll, 0);
        assert_eq!(dash.review_scroll, 0);
        assert!(!dash.search_mode);
        assert!(dash.search_query.is_empty());
        assert_eq!(dash.search_match_idx, 0);
    }

    #[test]
    fn detail_view_left_when_selected_stage_is_zero_no_op() {
        // Exercises the false branch of `if self.selected_stage > 0` (line 164).
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.selected_stage = 0;

        dash.handle_key(key(KeyCode::Left));

        assert_eq!(dash.selected_stage, 0);
    }

    #[test]
    fn detail_view_right_advances_stage_when_below_max() {
        // Exercises lines 178-185: the body of `if self.selected_stage < max_stage`
        // for the Right key in handle_detail_view_key.
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.num_stages = 3; // max_stage = 2
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.selected_stage = 0;
        dash.detail_scroll = 5;
        dash.review_scroll = 3;
        // search_mode must be FALSE so search mode doesn't intercept the Right key.
        dash.search_mode = false;
        dash.search_query = "bar".to_string();
        dash.search_match_idx = 7;

        dash.handle_key(key(KeyCode::Right));

        assert_eq!(dash.selected_stage, 1);
        assert_eq!(dash.detail_scroll, 0);
        assert_eq!(dash.review_scroll, 0);
        assert!(!dash.search_mode);
        assert!(dash.search_query.is_empty());
        assert_eq!(dash.search_match_idx, 0);
    }

    #[test]
    fn detail_view_right_at_max_stage_no_op() {
        // Exercises the false branch of `if self.selected_stage < max_stage`.
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.num_stages = 2; // max_stage = 1
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.selected_stage = 1; // already at max

        dash.handle_key(key(KeyCode::Right));

        assert_eq!(dash.selected_stage, 1);
    }

    // ─── detail view: number key stage jump ──────────────────────────────

    #[test]
    fn detail_view_number_key_jumps_to_valid_stage() {
        // Exercises lines 194-201: the body of `if idx <= max_stage` in the
        // Char('1'..='9') arm of handle_detail_view_key.
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.num_stages = 3; // max_stage=2; '3' => idx=2 which is <= 2
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.selected_stage = 0;
        dash.detail_scroll = 10;
        dash.review_scroll = 5;
        // search_mode must be FALSE — in search mode, Char('3') is treated as a
        // search character rather than a stage-jump key.
        dash.search_mode = false;
        dash.search_query = "xyz".to_string();
        dash.search_match_idx = 3;

        dash.handle_key(key(KeyCode::Char('3')));

        assert_eq!(dash.selected_stage, 2);
        assert_eq!(dash.detail_scroll, 0);
        assert_eq!(dash.review_scroll, 0);
        assert!(!dash.search_mode);
        assert!(dash.search_query.is_empty());
        assert_eq!(dash.search_match_idx, 0);
    }

    #[test]
    fn detail_view_number_key_out_of_range_no_op() {
        // Exercises the false branch of `if idx <= max_stage`: pressing '9'
        // when there are only 2 stages (max_stage=1) leaves selected_stage unchanged.
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.num_stages = 2; // max_stage=1; '9' => idx=8 which is > 1
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.selected_stage = 0;

        dash.handle_key(key(KeyCode::Char('9')));

        assert_eq!(dash.selected_stage, 0);
    }

    // ─── detail view: 'N' search when query is empty (no-op) ──────────────

    #[test]
    fn detail_view_shift_n_no_query_no_op() {
        // Exercises the false branch of `if !self.search_query.is_empty()` in
        // the Char('N') arm (line 301), ensuring search_match_idx stays at 0.
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.search_query.clear();
        dash.search_match_idx = 0;
        dash.handle_key(key(KeyCode::Char('N')));
        assert_eq!(dash.search_match_idx, 0);
    }

    // ─── handle_yank: no agent, is_run_state=false ────────────────────────

    #[test]
    fn yank_no_agent_is_noop() {
        // Exercises the `else` branch of `if let Some(agent) = self.selected_agent()`
        // (line 359) in handle_yank: no agent means nothing happens.
        let mut dash = make_test_dashboard();
        dash.detail_view = true;
        // No agents at all
        dash.handle_key(key(KeyCode::Char('y')));
        assert!(dash.toasts.is_empty());
    }

    #[test]
    fn yank_non_run_state_agent_is_noop() {
        // Exercises the `else` branch of `if agent.is_run_state` (line 358)
        // in handle_yank: non-run-state agents skip yank entirely.
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.is_run_state = false;
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;

        dash.handle_key(key(KeyCode::Char('y')));

        // No toast generated — yank does nothing for non-run-state agents.
        assert!(dash.toasts.is_empty());
    }

    // ─── process_events: agent not found for various event types ──────────

    #[test]
    fn process_events_stage_changed_unknown_agent_logs_but_no_update() {
        // Exercises the `else` path of `if let Some(agent) = ... .find(...)` (line 656)
        // in StageChanged: logs the message but no agent is updated.
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        let tx = dash.event_tx.clone();
        tx.send(AgentEvent::StageChanged {
            agent_id: "nonexistent".to_string(),
            stage: "code".to_string(),
        })
        .unwrap();
        dash.process_events();
        // Existing agent is untouched
        assert_eq!(dash.agents[0].stage, "main");
    }

    #[test]
    fn process_events_status_changed_unknown_agent_no_update() {
        // Exercises the `else` path of `if let Some(agent) = ... .find(...)` (line 662)
        // in StatusChanged: event for unknown agent id is silently ignored.
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        let tx = dash.event_tx.clone();
        tx.send(AgentEvent::StatusChanged {
            agent_id: "nonexistent".to_string(),
            status: AgentDisplayStatus::Complete,
        })
        .unwrap();
        dash.process_events();
        // Existing agent remains Active
        assert_eq!(dash.agents[0].status, AgentDisplayStatus::Active);
    }

    #[test]
    fn process_events_needs_input_unknown_agent_logs_but_no_update() {
        // Exercises the `else` path of `if let Some(agent) = ... .find(...)` (line 668)
        // in NeedsInput: logs the message but the unknown agent is not updated.
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        let tx = dash.event_tx.clone();
        tx.send(AgentEvent::NeedsInput {
            agent_id: "nonexistent".to_string(),
            prompt: "?".to_string(),
        })
        .unwrap();
        dash.process_events();
        // Existing agent still Active, no waiting_prompt set
        assert_eq!(dash.agents[0].status, AgentDisplayStatus::Active);
        assert!(dash.agents[0].waiting_prompt.is_none());
    }

    #[test]
    fn process_events_inference_complete_unknown_agent_logs_but_no_update() {
        // Exercises the `else` path of `if let Some(_agent) = ... .find(...)` (line 691)
        // in InferenceComplete: iteration is NOT incremented for unknown agents.
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        let tx = dash.event_tx.clone();
        tx.send(AgentEvent::InferenceComplete {
            agent_id: "nonexistent".to_string(),
            content: "done".to_string(),
            tokens_used: 10,
            tokens_prompt: 5,
        })
        .unwrap();
        dash.process_events();
        assert_eq!(dash.agents[0].iteration, 0);
    }

    #[test]
    fn process_events_error_unknown_agent_logs_but_no_update() {
        // Exercises the `else` path of `if let Some(agent) = ... .find(...)` (line 703)
        // in Error: the existing agent's status is unaffected.
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        let tx = dash.event_tx.clone();
        tx.send(AgentEvent::Error {
            agent_id: "nonexistent".to_string(),
            error: "boom".to_string(),
        })
        .unwrap();
        dash.process_events();
        assert_eq!(dash.agents[0].status, AgentDisplayStatus::Active);
    }

    // ─── main list: Down key — three agents to confirm can-move path ──────

    #[test]
    fn main_list_down_advances_through_multiple_agents() {
        // Exercises lines 421-424: the body of the Down key's inner if-block
        // in handle_main_list_key when selected < display_indices.len() - 1.
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.agents
            .push(make_test_agent("run-2", AgentDisplayStatus::Active));
        dash.agents
            .push(make_test_agent("run-3", AgentDisplayStatus::Active));
        dash.update_display_indices();
        assert_eq!(dash.selected, 0);

        dash.handle_key(key(KeyCode::Down));
        assert_eq!(dash.selected, 1);

        dash.handle_key(key(KeyCode::Down));
        assert_eq!(dash.selected, 2);

        // At bottom — cannot go further
        dash.handle_key(key(KeyCode::Down));
        assert_eq!(dash.selected, 2);
    }

    // ─── handle_cancel_from_list: pending_request cleared for run-state ───

    #[test]
    fn cancel_from_list_run_state_agent_clears_pending_request() {
        // Exercises line 493: `a.pending_request = None;` in handle_cancel_from_list.
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.is_run_state = true;
        agent.pending_request = Some(crate::interaction::InteractionRequest::free_text(
            "r1", "?", "main", true,
        ));
        dash.agents.push(agent);
        dash.update_display_indices();

        dash.handle_key(key(KeyCode::Char('c')));

        assert!(dash.agents[0].pending_request.is_none());
        assert_cancelled(&dash.agents[0].status);
    }

    // ─── handle_kill_from_list: pending_request cleared ───────────────────

    #[test]
    fn kill_from_list_run_state_agent_clears_pending_request() {
        // Exercises line 531: `a.pending_request = None;` in handle_kill_from_list.
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.is_run_state = true;
        agent.pending_request = Some(crate::interaction::InteractionRequest::free_text(
            "r1", "?", "main", true,
        ));
        dash.agents.push(agent);
        dash.update_display_indices();

        dash.handle_key(key(KeyCode::Char('k')));

        assert!(dash.agents[0].pending_request.is_none());
        assert_cancelled(&dash.agents[0].status);
    }

    // ─── handle_kill_from_detail: pending_request cleared ─────────────────

    #[test]
    fn kill_from_detail_run_state_agent_clears_pending_request() {
        // Exercises line 392: `a.pending_request = None;` in handle_kill_from_detail.
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.is_run_state = true;
        agent.pending_request = Some(crate::interaction::InteractionRequest::free_text(
            "r1", "?", "main", true,
        ));
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;

        dash.handle_key(key(KeyCode::Char('k')));

        assert!(dash.agents[0].pending_request.is_none());
        assert_cancelled(&dash.agents[0].status);
    }

    // ─── handle_yank_with_fn: clipboard-unavailable branch ───────────────────

    #[test]
    fn yank_clipboard_unavailable_shows_error_toast() {
        crate::runstate::with_isolated_runs_dir(
            "yank_clipboard_unavailable_shows_error_toast",
            |_d| {
                use crate::runstate;
                let run_id = "test-yank-clipboard-unavailable-x7z9";
                let stage_path = runstate::stage_dir(run_id, 0);
                std::fs::create_dir_all(&stage_path).ok();
                std::fs::write(stage_path.join("context.json"), r#"{"test":true}"#).ok();

                let mut dash = make_test_dashboard();
                let agent = make_test_agent(run_id, AgentDisplayStatus::Active);
                dash.agents.push(agent);
                dash.update_display_indices();
                dash.detail_view = true;
                dash.stage_content_mode = StageContentMode::Context;
                dash.selected_stage = 0;

                dash.handle_yank_with_fn(|_| false);

                assert!(dash
                    .toasts
                    .iter()
                    .any(|t| t.message.contains("Clipboard unavailable")));

                let _ = std::fs::remove_dir_all(runstate::run_dir(run_id));
            },
        );
    }
}
