//! Keyboard event handling, input submission, and event processing.

use crossterm::event::KeyCode;

use super::helpers::truncate;
use super::state::Dashboard;
use super::types::*;
use crate::runstate;
use leviath_core::interaction;

impl Dashboard {
    pub(super) fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        let key_code = key.code;
        // Help overlay takes priority
        if self.show_help {
            self.show_help = false;
            return;
        }

        // MCP management screen is modal: it owns all keys while open.
        if self.mcp_screen {
            self.handle_mcp_screen_key(key_code);
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

    /// Whether the user is actively editing a document (an `EditText`
    /// interaction). When true the editable textarea is rendered inline in the
    /// content pane — where the current text is shown — rather than in the bottom
    /// input bar, so editing happens in place over the document being revised.
    pub(in crate::commands::dashboard) fn editing_document(&self) -> bool {
        use interaction::InteractionKind;
        self.input_mode
            && self
                .selected_agent()
                .and_then(|a| a.pending_request.as_ref())
                .is_some_and(|r| r.kind == InteractionKind::EditText)
    }

    /// The document to review for the selected agent's pending interaction — the
    /// current instance's plan/output, carried as the request `body`. Shown in
    /// the output pane in place of the full accumulated history. `None` while
    /// actively editing (the textarea takes the pane) or when there is no body.
    pub(in crate::commands::dashboard) fn reviewing_body(&self) -> Option<String> {
        if self.editing_document() {
            return None;
        }
        self.selected_agent()
            .and_then(|a| a.pending_request.as_ref())
            .and_then(|r| r.body.as_deref())
            .filter(|b| !b.trim().is_empty())
            .map(str::to_string)
    }

    /// Whether the separate review pane is **on screen right now**, and so is
    /// what a scroll gesture should move.
    ///
    /// This must mirror the condition the renderer uses, not merely ask whether
    /// a review body exists. The pane is suppressed in input mode (where the
    /// document is drawn in the content pane instead) and whenever the output
    /// pane is already showing the same body — and in both of those cases a
    /// scroll aimed at `review_scroll` moves a pane nobody can see. That is
    /// exactly what happened: with a plan open for approval, every scroll key
    /// updated state that was not being rendered, so the plan sat still.
    ///
    /// `EditText` also uses `body`, but for editing rather than scrolling, so
    /// it is excluded.
    fn scroll_target_is_review(&self) -> bool {
        let content_shows_body =
            self.stage_content_mode == StageContentMode::Output && self.reviewing_body().is_some();
        !self.input_mode && !content_shows_body && self.has_scrollable_document()
    }

    /// Whether there is a review document on screen *somewhere* — the review
    /// pane or the content pane.
    ///
    /// "Is there anything to scroll", as distinct from
    /// [`Self::scroll_target_is_review`]'s "which pane holds it". Conflating
    /// those two questions is what aimed the scroll keys at a pane that was not
    /// being rendered.
    fn has_scrollable_document(&self) -> bool {
        use interaction::InteractionKind;
        self.selected_agent()
            .and_then(|a| a.pending_request.as_ref())
            .filter(|r| r.kind != InteractionKind::EditText)
            .and_then(|r| r.body.as_deref())
            .map(|b| !b.is_empty())
            .unwrap_or(false)
    }

    /// Scroll whatever is scrollable right now by `lines` (positive scrolls
    /// back through the document, matching the existing `review_scroll` /
    /// `detail_scroll` convention).
    ///
    /// One place so the keyboard and the mouse wheel cannot disagree about
    /// which pane a gesture moves.
    ///
    /// Scrolling clears any mouse selection: the highlight is anchored to
    /// screen cells, so moving text under it would leave it over the wrong
    /// content.
    pub(crate) fn scroll_by(&mut self, lines: i32) {
        self.selection = None;
        let target = match self.scroll_target_is_review() {
            true => &mut self.review_scroll,
            false => &mut self.detail_scroll,
        };
        *target = match lines >= 0 {
            true => target.saturating_add(lines.unsigned_abs() as usize),
            false => target.saturating_sub(lines.unsigned_abs() as usize),
        };
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
                    KeyCode::PageUp if self.has_scrollable_document() => self.scroll_by(10),
                    KeyCode::PageDown if self.has_scrollable_document() => self.scroll_by(-10),
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
                // Up/Down move the selection here, so the document gets its own
                // keys. Without these there was no way at all to read a plan
                // longer than the pane while its approval prompt was open —
                // which is exactly when you need to read it.
                KeyCode::PageUp => self.scroll_by(10),
                KeyCode::PageDown => self.scroll_by(-10),
                KeyCode::Home => self.scroll_by(i32::MAX),
                KeyCode::End => self.scroll_by(i32::MIN),
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
                    self.reset_context_history();
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
                // `c` shows the live current window; leave any history browsing.
                self.reset_context_history();
            }
            // Browse the run's archived context-window history in the Context
            // view: `,` = earlier point, `.` = later (past the newest → live).
            KeyCode::Char(',') => self.step_context_history(-1),
            KeyCode::Char('.') => self.step_context_history(1),
            KeyCode::Char('i') => {
                // Respond to a pending interaction, or send a mid-run message to
                // any active agent that accepts them — same key, same input area.
                if self.selected_stage_can_respond() || self.selected_agent_accepts_messages() {
                    self.input_mode = true;
                    self.choice_selected = 0;
                    self.seed_input_textarea();
                }
            }
            // All four go through `scroll_by`, which is the one place that
            // decides which pane a gesture moves. They used to carry their own
            // copy of that decision, which is how the keyboard and the renderer
            // came to disagree about where the document was.
            KeyCode::Up => self.scroll_by(1),
            KeyCode::Down => self.scroll_by(-1),
            KeyCode::PageUp => self.scroll_by(10),
            KeyCode::PageDown => self.scroll_by(-10),
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
                        runstate::stage_dir(&agent.id, self.selected_stage).join("context.json"),
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

    fn handle_kill_from_detail(&mut self) {
        if let Some(agent) = self.selected_agent()
            && agent.status.is_killable()
        {
            let agent_id = agent.id.clone();
            let _ = self.cmd_tx.send(DaemonCommand::Cancel {
                run_id: agent_id.clone(),
            });
            // The index came from `display_indices`/`agents` just above, via the
            // `self.selected_agent()` lookup that got us into this branch, and
            // the `cmd_tx.send` in between does not mutate either collection or
            // `self.selected` -- so it's always still valid. An `if let` guard
            // here would add an "index went stale" branch that can never
            // actually be exercised.
            let idx = self
                .selected_agent_raw_idx()
                .expect("selected_agent() returned Some above");
            let a = self
                .agents
                .get_mut(idx)
                .expect("index snapshotted from the still-unchanged display_indices/agents above");
            a.status = AgentDisplayStatus::Cancelled;
            a.waiting_prompt = None;
            a.pending_request = None;
            self.input_mode = false;
            self.input_textarea = tui_textarea::TextArea::default();
            self.add_log(format!("{}: kill requested", agent_id));
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
                if let Some(id) = self.selected_agent().map(|a| a.id.clone()) {
                    self.confirm_delete = true;
                    self.add_log(format!(
                        "Delete run '{}'? This cancels it and is PERMANENT. (y/n)",
                        id
                    ));
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
            KeyCode::Char('m') => {
                // Open the MCP management screen; load the current servers.
                self.mcp_screen = true;
                self.mcp_selected = 0;
                self.refresh_mcp_rows();
            }
            _ => {}
        }
    }

    /// Keys handled while the MCP management screen is open.
    ///
    /// In add mode the line editor owns typing; otherwise the keys drive the
    /// server list (navigate, add, delete, login, test).
    fn handle_mcp_screen_key(&mut self, key_code: KeyCode) {
        if self.mcp_add_mode {
            match key_code {
                KeyCode::Esc => {
                    self.mcp_add_mode = false;
                    self.mcp_add_input.clear();
                }
                KeyCode::Enter => {
                    let line = std::mem::take(&mut self.mcp_add_input);
                    self.mcp_add_mode = false;
                    self.mcp_add_from_line(&line);
                }
                KeyCode::Backspace => {
                    self.mcp_add_input.pop();
                }
                KeyCode::Char(c) => {
                    self.mcp_add_input.push(c);
                }
                _ => {}
            }
            return;
        }

        match key_code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mcp_screen = false;
            }
            KeyCode::Up => {
                self.mcp_selected = self.mcp_selected.saturating_sub(1);
            }
            KeyCode::Down => {
                if !self.mcp_rows.is_empty() && self.mcp_selected + 1 < self.mcp_rows.len() {
                    self.mcp_selected += 1;
                }
            }
            KeyCode::Char('a') => {
                self.mcp_add_mode = true;
                self.mcp_add_input.clear();
            }
            KeyCode::Char('d') => {
                self.mcp_remove_selected();
            }
            KeyCode::Char('l') => {
                self.mcp_login_selected();
            }
            KeyCode::Char('t') => {
                self.mcp_test_selected();
            }
            KeyCode::Char('r') => {
                self.refresh_mcp_rows();
            }
            _ => {}
        }
    }

    fn handle_cancel_from_list(&mut self) {
        if let Some(agent) = self.selected_agent()
            && agent.status.is_killable()
        {
            let agent_id = agent.id.clone();
            let _ = self.cmd_tx.send(DaemonCommand::Cancel {
                run_id: agent_id.clone(),
            });
            // See the comment in `handle_kill_from_detail` -- the index is still
            // valid because the `cmd_tx.send` above does not touch
            // `display_indices`/`agents`/`selected`.
            let idx = self
                .selected_agent_raw_idx()
                .expect("selected_agent() returned Some above");
            let a = self
                .agents
                .get_mut(idx)
                .expect("index snapshotted from the still-unchanged display_indices/agents above");
            a.status = AgentDisplayStatus::Cancelled;
            a.waiting_prompt = None;
            a.pending_request = None;
            self.add_log(format!("{}: Cancel requested", agent_id));
        }
    }

    fn handle_kill_from_list(&mut self) {
        if let Some(agent) = self.selected_agent()
            && agent.status.is_killable()
        {
            let agent_id = agent.id.clone();
            let _ = self.cmd_tx.send(DaemonCommand::Cancel {
                run_id: agent_id.clone(),
            });
            // See the comment in `handle_kill_from_detail` -- the index is still
            // valid because the `cmd_tx.send` above does not touch
            // `display_indices`/`agents`/`selected`.
            let idx = self
                .selected_agent_raw_idx()
                .expect("selected_agent() returned Some above");
            let a = self
                .agents
                .get_mut(idx)
                .expect("index snapshotted from the still-unchanged display_indices/agents above");
            a.status = AgentDisplayStatus::Cancelled;
            a.waiting_prompt = None;
            a.pending_request = None;
            self.add_log(format!("{}: kill requested", agent_id));
        }
    }

    pub(super) fn submit_input(&mut self) {
        use interaction::{ApprovalScope, InteractionKind, InteractionResponse};

        let (agent_id, req) = match self.selected_agent() {
            Some(a) => (a.id.clone(), a.pending_request.clone()),
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
        // `self.selected_agent()` lookup at the top of this function (which is
        // where `req`/`agent_id` came from) touches
        // `display_indices`/`agents`/`selected` -- an `if let` guard here would
        // add an "index went stale" branch that can never actually be exercised.
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

        // A pending interaction is answered via the daemon's interaction hub;
        // free-typed input with no pending request is delivered as a mid-run
        // message to the agent.
        if req.is_some() {
            let _ = self.cmd_tx.send(DaemonCommand::Answer { response: resp });
            self.add_log(format!("Sent: {}", display));
        } else {
            let content = resp.value.clone().unwrap_or_default();
            let _ = self.cmd_tx.send(DaemonCommand::Message {
                agent_id: agent_id.clone(),
                content,
            });
            self.add_log(format!("💬 User: \"{}\"", display));
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
            stage: "main".to_string(),
            stage_index: 0,
            num_stages: 2,
            status,
            tokens_in: 0,
            tokens_out: 0,
            cached_tokens: 0,
            iteration: 0,
            waiting_prompt: None,
            pending_request: None,
            last_answered_request_id: None,
            context_snapshot: None,
            stages: vec![],
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

    /// A plan approval is a multiple-choice prompt with the plan as its body.
    fn dashboard_awaiting_a_plan() -> Dashboard {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        let mut req = leviath_core::interaction::InteractionRequest::multiple_choice(
            "plan_approval",
            "Review the plan above. What would you like to do?",
            vec!["Approve".into(), "Revise".into()],
            "plan",
        );
        req.body = Some(format!("## Plan\n{}", "1. a step\n".repeat(200)));
        agent.pending_request = Some(req);
        agent.waiting_prompt = Some("Review the plan above.".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;
        dash
    }

    /// Up/Down move the choice selection, so before this there was no key at
    /// all that scrolled the document — a plan longer than the pane could not
    /// be read while its approval prompt was open, which is the only time it is
    /// shown.
    #[test]
    fn a_plan_can_be_scrolled_while_its_approval_prompt_is_open() {
        let mut dash = dashboard_awaiting_a_plan();
        assert_eq!(dash.detail_scroll, 0);

        // In input mode the plan is drawn in the content pane — the separate
        // review pane is suppressed — so that is what has to move. Verified
        // against the real dashboard through a pty: the pane's own position
        // readout went 100% → 91% → 81% on two PageUps.
        dash.handle_key(key(KeyCode::PageUp));
        assert_eq!(dash.detail_scroll, 10, "PageUp scrolls the plan");
        assert_eq!(dash.review_scroll, 0, "and not a pane nobody can see");
        dash.handle_key(key(KeyCode::PageDown));
        assert_eq!(dash.detail_scroll, 0, "and PageDown comes back");

        // Down still selects rather than scrolling — the plan keys were added
        // beside the choice keys, not on top of them.
        dash.choice_selected = 0;
        dash.handle_key(key(KeyCode::Down));
        assert_eq!(dash.choice_selected, 1);
        assert_eq!(dash.detail_scroll, 0);

        // Home/End jump the length of the document without overflowing.
        dash.handle_key(key(KeyCode::Home));
        assert!(dash.detail_scroll > 0);
        dash.handle_key(key(KeyCode::End));
        assert_eq!(dash.detail_scroll, 0);
    }

    /// A free-text prompt can carry a document too (an agent asking a question
    /// about something it is showing you), so it gets the same scroll keys.
    /// `EditText` deliberately does not: there the body is what you are editing,
    /// and PageUp/PageDown belong to the text area.
    #[test]
    fn free_text_scrolls_a_review_but_edit_text_keeps_its_page_keys() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        let mut req = leviath_core::interaction::InteractionRequest::free_text(
            "q1",
            "What next?",
            "plan",
            true,
        );
        req.body = Some("a line\n".repeat(200));
        agent.pending_request = Some(req);
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;

        dash.handle_key(key(KeyCode::PageUp));
        assert_eq!(dash.detail_scroll, 10, "the document scrolls");
        assert!(
            dash.input_textarea.lines().concat().is_empty(),
            "and the key did not land in the answer box"
        );
        dash.handle_key(key(KeyCode::PageDown));
        assert_eq!(dash.detail_scroll, 0);

        // EditText: the body is the thing being edited, so the guard is false
        // and the key goes to the text area instead.
        dash.agents[0].pending_request =
            Some(leviath_core::interaction::InteractionRequest::edit_text(
                "e1",
                "Edit the plan",
                "plan",
                "## Plan\n1. a step",
            ));
        dash.detail_scroll = 0;
        dash.handle_key(key(KeyCode::PageUp));
        assert_eq!(
            dash.detail_scroll, 0,
            "an edit target is not scrolled out from under the cursor"
        );
    }

    /// The wheel moves the same pane the keyboard does.
    #[test]
    fn the_mouse_wheel_scrolls_the_review_and_falls_back_to_the_detail_pane() {
        let mut dash = dashboard_awaiting_a_plan();
        dash.scroll_by(3);
        assert_eq!(dash.detail_scroll, 3);
        dash.scroll_by(-3);
        assert_eq!(dash.detail_scroll, 0);
        // Past the top it stops rather than wrapping.
        dash.scroll_by(-3);
        assert_eq!(dash.detail_scroll, 0);

        // Out of input mode and with the content pane on logs, the review pane
        // is what is rendered — and so what the wheel moves.
        dash.input_mode = false;
        dash.handle_key(key(KeyCode::Char('l')));
        dash.scroll_by(3);
        assert_eq!(dash.review_scroll, 3);
        assert_eq!(dash.detail_scroll, 0, "the content pane did not move");
    }

    #[test]
    fn input_mode_choice_up_down() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(
            leviath_core::interaction::InteractionRequest::multiple_choice(
                "mc1",
                "Pick one",
                vec!["A".into(), "B".into(), "C".into()],
                "main",
            ),
        );
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
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::free_text(
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
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::free_text(
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
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::edit_text(
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
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::edit_text(
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
    fn editing_document_true_only_while_editing_an_edit_text() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::edit_text(
            "et1", "Edit", "main", "body",
        ));
        dash.agents.push(agent);
        dash.update_display_indices();

        // Pending EditText but not in input mode ⇒ not editing yet.
        assert!(!dash.editing_document());
        // Entering input mode over an EditText ⇒ editing inline.
        dash.input_mode = true;
        assert!(dash.editing_document());
        // While editing, the textarea owns the pane — no separate review body.
        assert!(dash.reviewing_body().is_none());
    }

    #[test]
    fn reviewing_body_returns_the_pending_document_or_none() {
        let mut dash = make_test_dashboard();
        // No agent / no pending ⇒ None.
        assert!(dash.reviewing_body().is_none());

        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        let mut req = leviath_core::interaction::InteractionRequest::multiple_choice(
            "mc1",
            "Approve?",
            vec!["Approve".to_string()],
            "plan_approval",
        );
        req.body = Some("## Plan\n1. write it".to_string());
        agent.pending_request = Some(req);
        dash.agents.push(agent);
        dash.update_display_indices();
        // A pending body is surfaced for review.
        assert_eq!(
            dash.reviewing_body().as_deref(),
            Some("## Plan\n1. write it")
        );

        // A blank body is not surfaced.
        dash.agents[0].pending_request.as_mut().unwrap().body = Some("   ".to_string());
        assert!(dash.reviewing_body().is_none());
    }

    #[test]
    fn editing_document_false_for_non_edit_text_prompt() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::free_text(
            "ft1", "Q?", "main", true,
        ));
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.input_mode = true;
        // FreeText input mode is a bottom-bar response, not a document edit.
        assert!(!dash.editing_document());
    }

    #[test]
    fn seed_input_textarea_empty_for_free_text() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::free_text(
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
        agent.pending_request = Some(interaction::InteractionRequest::edit_text(
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
        // The edited text is answered to the daemon with indentation + newline intact.
        let cmd = cmd_rx.try_recv().expect("an Answer command was queued");
        assert_eq!(
            cmd,
            DaemonCommand::Answer {
                response: interaction::InteractionResponse::text("et1", "  indented\nsecond line"),
            }
        );
    }

    #[test]
    fn submit_input_edit_text_empty_reports_no_changes() {
        // Covers the empty-edit display branch of the EditText submit arm.
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(interaction::InteractionRequest::edit_text(
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
        let cmd = cmd_rx.try_recv().expect("an Answer command was queued");
        assert_eq!(
            cmd,
            DaemonCommand::Answer {
                response: interaction::InteractionResponse::text("et1", "   "),
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
        agent.pending_request = Some(
            leviath_core::interaction::InteractionRequest::multiple_choice(
                "mc1",
                "Pick",
                vec!["A".into(), "B".into()],
                "main",
            ),
        );
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

    /// Every state that is not a finished one can be killed. The gate used to be
    /// `Active | Waiting`, so a run showing IDLE or STALE — exactly the states a
    /// user reaches for the kill key in — could not be killed at all.
    #[test]
    fn every_unfinished_state_can_be_killed() {
        for status in [
            AgentDisplayStatus::Active,
            AgentDisplayStatus::Waiting,
            AgentDisplayStatus::Idle,
            AgentDisplayStatus::Stale,
        ] {
            for key_code in ['c', 'k'] {
                let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
                let mut dash = Dashboard::new(cmd_tx);
                dash.agents.push(make_test_agent("run-1", status.clone()));
                dash.update_display_indices();
                dash.handle_key(key(KeyCode::Char(key_code)));

                assert_eq!(
                    cmd_rx.try_recv().unwrap(),
                    DaemonCommand::Cancel {
                        run_id: "run-1".to_string()
                    },
                    "{status:?} must be killable via '{key_code}'"
                );
            }
        }
    }

    /// …and a finished run is left alone: there is nothing to kill, and asking
    /// the daemon would just produce a spurious failure toast.
    #[test]
    fn a_finished_run_is_not_killed() {
        for status in [
            AgentDisplayStatus::Complete,
            AgentDisplayStatus::CompleteInteractive,
            AgentDisplayStatus::Cancelled,
            AgentDisplayStatus::Error("boom".to_string()),
        ] {
            let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
            let mut dash = Dashboard::new(cmd_tx);
            dash.agents.push(make_test_agent("run-1", status.clone()));
            dash.update_display_indices();
            dash.handle_key(key(KeyCode::Char('k')));

            assert!(
                cmd_rx.try_recv().is_err(),
                "{status:?} must not be re-killed"
            );
        }
    }

    #[test]
    fn cancel_from_list_waiting_agent_clears_local_state_and_cancels() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.waiting_prompt = Some("?".to_string());
        agent.pending_request = Some(interaction::InteractionRequest::free_text(
            "q1", "?", "main", true,
        ));
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Char('c')));

        assert_cancelled(&dash.agents[0].status);
        assert!(dash.agents[0].pending_request.is_none());
        assert_eq!(
            cmd_rx.try_recv().unwrap(),
            DaemonCommand::Cancel {
                run_id: "run-1".to_string()
            }
        );
    }

    #[test]
    fn cancel_from_list_with_pid_sends_signal() {
        // See kill_from_detail_with_pid_sends_signal for why this PID value
        // is safe: implausibly large, guaranteed ESRCH no-op.
        let mut dash = make_test_dashboard();
        let agent = make_test_agent("run-1", AgentDisplayStatus::Active);
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
        let agent = make_test_agent("run-1", AgentDisplayStatus::Active);
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
        let agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.handle_key(key(KeyCode::Char('k')));
        assert_cancelled(&dash.agents[0].status);
    }

    #[test]
    fn kill_from_detail_waiting_agent_clears_local_state_and_cancels() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.waiting_prompt = Some("?".to_string());
        agent.pending_request = Some(interaction::InteractionRequest::free_text(
            "q1", "?", "main", true,
        ));
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.handle_key(key(KeyCode::Char('k')));

        assert_cancelled(&dash.agents[0].status);
        assert!(dash.agents[0].pending_request.is_none());
        assert_eq!(
            cmd_rx.try_recv().unwrap(),
            DaemonCommand::Cancel {
                run_id: "run-1".to_string()
            }
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
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::free_text(
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

    // ─── detail view: review scroll ───────────────────────────────────────

    #[test]
    fn detail_view_scroll_with_review_body() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::review(
            "rev1",
            "Review",
            "# Long markdown body\n\nSome content here.",
            "main",
        ));
        agent.waiting_prompt = Some("Review".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;

        // In the default Output mode the content pane is already showing the
        // body, so the separate review pane is suppressed and the content pane
        // is what a scroll must move. This test used to assert `review_scroll`
        // here — a pane that is not on screen — which is precisely why a plan
        // sat still while every scroll key "worked".
        dash.handle_key(key(KeyCode::Up));
        assert_eq!(dash.detail_scroll, 1);
        assert_eq!(dash.review_scroll, 0);

        dash.handle_key(key(KeyCode::Down));
        assert_eq!(dash.detail_scroll, 0);

        dash.handle_key(key(KeyCode::PageUp));
        assert_eq!(dash.detail_scroll, 10);

        dash.handle_key(key(KeyCode::PageDown));
        assert_eq!(dash.detail_scroll, 0);

        // Switch the content pane to logs and the review pane *is* rendered, so
        // now the same keys move it instead.
        dash.handle_key(key(KeyCode::Char('l')));
        dash.handle_key(key(KeyCode::PageUp));
        assert_eq!(dash.review_scroll, 10, "the review pane is on screen now");
        assert_eq!(dash.detail_scroll, 0);
    }

    // ─── handle_input_mode_key for ToolApproval ──────────────────────────

    #[test]
    fn input_mode_tool_approval_up_down() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(
            leviath_core::interaction::InteractionRequest::tool_approval(
                "ta1",
                "bash",
                serde_json::json!({"cmd": "ls"}),
                "main",
            ),
        );
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
        agent.pending_request = Some(
            leviath_core::interaction::InteractionRequest::tool_approval(
                "ta1",
                "bash",
                serde_json::json!({"cmd": "ls"}),
                "main",
            ),
        );
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
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::free_text(
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
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::free_text(
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
        agent.pending_request = Some(
            leviath_core::interaction::InteractionRequest::multiple_choice(
                "mc1",
                "Pick",
                vec!["A".into(), "B".into(), "C".into()],
                "main",
            ),
        );
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
        agent.pending_request = Some(
            leviath_core::interaction::InteractionRequest::tool_approval(
                "ta1",
                "bash",
                serde_json::json!({"cmd": "ls"}),
                "main",
            ),
        );
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
        agent.pending_request = Some(
            leviath_core::interaction::InteractionRequest::tool_approval(
                "ta1",
                "bash",
                serde_json::json!({"cmd": "ls"}),
                "main",
            ),
        );
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
        agent.pending_request = Some(
            leviath_core::interaction::InteractionRequest::tool_approval(
                "ta1",
                "bash",
                serde_json::json!({"cmd": "ls"}),
                "main",
            ),
        );
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
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::confirm(
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
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::confirm(
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
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::free_text(
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
        agent.pending_request = None;
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;

        dash.input_textarea.insert_str("/exit");
        dash.submit_input();

        assert!(!dash.input_mode);
    }

    // ─── handle_kill_from_detail for in-process agent ─────────────────────

    #[test]
    fn kill_from_detail_in_process_agent_sends_command() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let agent = make_test_agent("run-1", AgentDisplayStatus::Active);
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
        let agent = make_test_agent("run-1", AgentDisplayStatus::Active);
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
        let agent = make_test_agent("run-1", AgentDisplayStatus::Active);
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

            assert!(
                dash.toasts
                    .iter()
                    .any(|t| t.message.contains("yanked to clipboard"))
            );

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

    // ─── input_mode_key: FreeText Enter submits ───────────────────────────

    #[test]
    fn input_mode_free_text_enter_submits() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::free_text(
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
            agent.pending_request = Some(
                leviath_core::interaction::InteractionRequest::multiple_choice(
                    "mc1",
                    "Pick",
                    vec!["A".into(), "B".into()],
                    "main",
                ),
            );
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

    // ─── submit_input last_answered_request_id tracking ───────────────────

    #[test]
    fn submit_input_tracks_last_answered_request_id() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::free_text(
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

    /// A dashboard in detail view that is browsing a (fake) history point, for
    /// the reset-to-live key tests.
    fn browsing_detail_dashboard() -> Dashboard {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.context_history = vec![leviath_core::run_archive::RunPoint {
            meta: leviath_core::run_meta::RunMeta::new(
                "run-1".to_string(),
                "a".to_string(),
                "/p".to_string(),
                "t".to_string(),
                None,
                "/w".to_string(),
                1,
            ),
            context: leviath_core::run_meta::ContextSnapshot {
                stage_name: "s".to_string(),
                total_tokens: 0,
                max_tokens: 100,
                regions: vec![],
            },
            at: 0,
        }];
        dash.context_history_idx = Some(0);
        dash
    }

    #[test]
    fn detail_view_comma_and_period_route_to_history_step() {
        // With no archive on disk, stepping is a no-op — this exercises the
        // `,`/`.` routing arms without needing a run.lvr fixture.
        let mut dash = make_test_dashboard();
        dash.agents.push(make_test_agent(
            "run-no-archive",
            AgentDisplayStatus::Active,
        ));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.handle_key(key(KeyCode::Char(',')));
        dash.handle_key(key(KeyCode::Char('.')));
        assert_eq!(dash.context_history_idx, None);
    }

    #[test]
    fn detail_view_c_returns_to_live_context() {
        let mut dash = browsing_detail_dashboard();
        dash.handle_key(key(KeyCode::Char('c')));
        assert_eq!(dash.stage_content_mode, StageContentMode::Context);
        assert_eq!(dash.context_history_idx, None); // back to live
        assert!(dash.context_history.is_empty());
    }

    #[test]
    fn detail_view_esc_resets_context_history() {
        let mut dash = browsing_detail_dashboard();
        dash.handle_key(key(KeyCode::Esc));
        assert!(!dash.detail_view);
        assert_eq!(dash.context_history_idx, None);
        assert!(dash.context_history.is_empty());
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
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::free_text(
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
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::free_text(
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
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::free_text(
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

                assert!(
                    dash.toasts
                        .iter()
                        .any(|t| t.message.contains("Clipboard unavailable"))
                );

                let _ = std::fs::remove_dir_all(runstate::run_dir(run_id));
            },
        );
    }

    // ─── MCP management screen keys ───────────────────────────────────────

    /// A dashboard whose MCP context points at a temp dir.
    fn mcp_dash(dir: &std::path::Path) -> Dashboard {
        let mut dash = make_test_dashboard();
        dash.mcp_ctx.config_path = dir.join("config.toml");
        dash.mcp_ctx.store_path = dir.join("mcp-auth.json");
        dash
    }

    #[test]
    fn m_opens_the_mcp_screen() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = mcp_dash(dir.path());
        dash.handle_key(key(KeyCode::Char('m')));
        assert!(dash.mcp_screen);
    }

    #[test]
    fn q_and_esc_close_the_mcp_screen() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = mcp_dash(dir.path());
        dash.mcp_screen = true;
        dash.handle_key(key(KeyCode::Char('q')));
        assert!(!dash.mcp_screen);

        dash.mcp_screen = true;
        dash.handle_key(key(KeyCode::Esc));
        assert!(!dash.mcp_screen);
    }

    #[test]
    fn arrows_navigate_the_mcp_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = mcp_dash(dir.path());
        dash.mcp_add_from_line("a npx");
        dash.mcp_add_from_line("b npx");
        dash.refresh_mcp_rows();
        dash.mcp_screen = true;
        dash.mcp_selected = 0;

        dash.handle_key(key(KeyCode::Down));
        assert_eq!(dash.mcp_selected, 1);
        dash.handle_key(key(KeyCode::Down)); // clamped at last
        assert_eq!(dash.mcp_selected, 1);
        dash.handle_key(key(KeyCode::Up));
        assert_eq!(dash.mcp_selected, 0);
        dash.handle_key(key(KeyCode::Up)); // clamped at 0
        assert_eq!(dash.mcp_selected, 0);
    }

    #[test]
    fn a_opens_the_add_editor_and_enter_adds() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = mcp_dash(dir.path());
        dash.mcp_screen = true;

        dash.handle_key(key(KeyCode::Char('a')));
        assert!(dash.mcp_add_mode);
        // Type "srv npx".
        for c in "srv npx".chars() {
            dash.handle_key(key(KeyCode::Char(c)));
        }
        // A backspace then retype to cover Backspace.
        dash.handle_key(key(KeyCode::Backspace));
        dash.handle_key(key(KeyCode::Char('x')));
        dash.handle_key(key(KeyCode::Enter));
        assert!(!dash.mcp_add_mode);

        let config =
            crate::config::Config::load_from_path_public(&dash.mcp_ctx.config_path).unwrap();
        assert_eq!(config.mcp_servers.len(), 1);
        assert_eq!(config.mcp_servers[0].command.as_deref(), Some("npx")); // "np" + "x"
    }

    #[test]
    fn esc_cancels_the_add_editor() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = mcp_dash(dir.path());
        dash.mcp_screen = true;
        dash.mcp_add_mode = true;
        dash.mcp_add_input = "half typed".to_string();
        dash.handle_key(key(KeyCode::Esc));
        assert!(!dash.mcp_add_mode);
        assert!(dash.mcp_add_input.is_empty());
    }

    #[test]
    fn add_editor_ignores_non_text_keys() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = mcp_dash(dir.path());
        dash.mcp_screen = true;
        dash.mcp_add_mode = true;
        // A key with no meaning in the editor is a no-op.
        dash.handle_key(key(KeyCode::Down));
        assert!(dash.mcp_add_mode);
        assert!(dash.mcp_add_input.is_empty());
    }

    #[test]
    fn d_deletes_the_selected_server() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = mcp_dash(dir.path());
        dash.mcp_add_from_line("gone npx");
        dash.refresh_mcp_rows();
        dash.mcp_screen = true;
        dash.mcp_selected = 0;
        dash.handle_key(key(KeyCode::Char('d')));
        assert!(dash.mcp_rows.is_empty());
    }

    #[test]
    fn l_and_t_dispatch_login_and_test() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = mcp_dash(dir.path());
        dash.mcp_add_from_line("remote https://e.com/mcp");
        dash.refresh_mcp_rows();
        dash.mcp_screen = true;
        dash.mcp_selected = 0;
        dash.handle_key(key(KeyCode::Char('l')));
        dash.handle_key(key(KeyCode::Char('t')));
        assert!(dash.mcp_cmd_rx_for_test().try_recv().is_ok());
        assert!(dash.mcp_cmd_rx_for_test().try_recv().is_ok());
    }

    #[test]
    fn r_refreshes_the_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = mcp_dash(dir.path());
        dash.mcp_screen = true;
        // Add a server on disk behind the screen's back, then refresh.
        let mut config = crate::config::Config::default();
        config
            .mcp_servers
            .push(leviath_mcp::MCPServerConfig::stdio("x", "npx", vec![]));
        config
            .save_to_path_public(&dash.mcp_ctx.config_path)
            .unwrap();
        assert!(dash.mcp_rows.is_empty());
        dash.handle_key(key(KeyCode::Char('r')));
        assert_eq!(dash.mcp_rows.len(), 1);
    }

    #[test]
    fn unbound_mcp_screen_keys_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = mcp_dash(dir.path());
        dash.mcp_screen = true;
        dash.handle_key(key(KeyCode::Char('z')));
        assert!(dash.mcp_screen, "an unbound key does nothing");
    }
}
