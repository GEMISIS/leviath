//! Keyboard event handling, input submission, and event processing.

use crossterm::event::KeyCode;

use super::helpers::truncate;
use super::state::Dashboard;
use super::types::*;
use crate::runstate;
use crate::tui::widgets::markdown_edit::MarkdownEdit;
use leviath_core::interaction;

impl Dashboard {
    pub(super) fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        let key_code = key.code;
        // Ctrl-C quits from anywhere - dialogs, text inputs, every screen. It
        // is the one binding a user reaches for expecting it to always work.
        if key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
            && key_code == KeyCode::Char('c')
        {
            self.should_quit = true;
            return;
        }

        // Help overlay: scrolls, and closes deliberately (Esc/q/?/Enter),
        // never by a key that would also act underneath.
        if self.show_help {
            if crate::tui::widgets::help::handle_help_key(&key, &self.help_scroll) {
                self.show_help = false;
            }
            return;
        }

        // A confirmation dialog owns the keys until answered.
        if self.pending_confirm.is_some() {
            self.handle_confirm_key(&key);
            return;
        }

        // The Agents screen (catalog, chooser, editor) is modal too.
        if self.agent_builder.is_some() {
            self.handle_agents_key(key);
            return;
        }

        // MCP management screen is modal: it owns all keys while open.
        if self.mcp_screen {
            self.handle_mcp_screen_key(key_code);
            return;
        }

        // So is the new-run screen. It has to be: a task is typed here, so
        // every letter belongs to the editor rather than to a list command.
        if self.new_run_screen {
            self.handle_new_run_key(key);
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

            // Detail view - not in input mode
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

        if self.main_focus == MainPane::LogPane {
            self.handle_log_pane_key(key_code);
            return;
        }
        self.handle_main_list_key(key_code);
    }

    /// Keys while a confirmation dialog is open: the dialog decides, then the
    /// confirmed action runs against the run id it was opened for.
    fn handle_confirm_key(&mut self, key: &crossterm::event::KeyEvent) {
        use crate::tui::widgets::confirm::ConfirmOutcome;
        let Some((action, mut dialog)) = self.pending_confirm.take() else {
            return;
        };
        match dialog.handle(key) {
            ConfirmOutcome::Pending => self.pending_confirm = Some((action, dialog)),
            ConfirmOutcome::No => self.add_log("Cancelled".to_string()),
            ConfirmOutcome::Yes => match action {
                ConfirmAction::Kill { run_ids } => {
                    for run_id in &run_ids {
                        // Un-mark first: a killed row stays in the list, and a
                        // mark that survived the kill would silently pull the
                        // run into the next group action.
                        self.marked.remove(run_id);
                        self.perform_kill(run_id);
                    }
                }
                ConfirmAction::Delete { run_ids } => {
                    for run_id in &run_ids {
                        self.marked.remove(run_id);
                        self.perform_delete(run_id);
                    }
                }
                ConfirmAction::McpRemove { name } => self.mcp_remove_named(&name),
                // The box is read off the dialog rather than carried in the
                // outcome: whether to ask again is a note about future
                // questions, not a third answer to this one.
                ConfirmAction::EnableYolo => self.accept_yolo_warning(dialog.remembered()),
                ConfirmAction::AgentDelete { name } => self.perform_agent_delete(&name),
                ConfirmAction::AgentReset { name } => self.perform_agent_reset(&name),
                ConfirmAction::StageDelete { name } => self.editor_delete_stage(&name),
                ConfirmAction::EditorDiscard => self.close_editor(),
                ConfirmAction::RegionDelete { scope, name } => {
                    self.editor_delete_region(&scope, &name)
                }
                ConfirmAction::OverrideRemove { stage } => self.editor_remove_override(&stage),
            },
        }
    }

    /// Keys while the log panel holds focus: scroll history, End/G resumes
    /// tailing, Tab/Esc hand focus back to the run list.
    fn handle_log_pane_key(&mut self, key_code: KeyCode) {
        let len = self.log.len();
        let viewport = self.log_viewport.max(1);
        match key_code {
            KeyCode::Up | KeyCode::Char('k') => self.log_scroll.scroll_up(1, len, viewport),
            KeyCode::Down | KeyCode::Char('j') => self.log_scroll.scroll_down(1),
            KeyCode::PageUp => self.log_scroll.scroll_up(viewport, len, viewport),
            KeyCode::PageDown => self.log_scroll.scroll_down(viewport),
            KeyCode::Home | KeyCode::Char('g') => self.log_scroll.jump_to_top(len, viewport),
            KeyCode::End | KeyCode::Char('G') => self.log_scroll.jump_to_tail(),
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Esc => {
                self.main_focus = MainPane::RunList;
            }
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') | KeyCode::F(1) => self.show_help = true,
            _ => {}
        }
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
        let mode = self.md_mode();
        self.input_textarea = match seed {
            Some(body) => MarkdownEdit::from_text(&body),
            None => MarkdownEdit::default(),
        }
        .in_mode(mode);
    }

    /// Whether the user is actively editing a document (an `EditText`
    /// interaction). When true the editable textarea is rendered inline in the
    /// content pane - where the current text is shown - rather than in the bottom
    /// input bar, so editing happens in place over the document being revised.
    pub(in crate::commands::dashboard) fn editing_document(&self) -> bool {
        use interaction::InteractionKind;
        self.input_mode
            && self
                .selected_agent()
                .and_then(|a| a.pending_request.as_ref())
                .is_some_and(|r| r.kind == InteractionKind::EditText)
    }

    /// The document to review for the selected agent's pending interaction - the
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
    /// pane is already showing the same body - and in both of those cases a
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

    /// Whether there is a review document on screen *somewhere* - the review
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

        // The response box's own popup outranks this screen's keys, so Enter
        // finishes the link rather than sending the answer.
        if self.input_textarea.is_modal() {
            let outcome = self.input_textarea.handle_key(&key);
            self.remember_md_mode(outcome);
            return;
        }
        match &kind {
            Some(InteractionKind::FreeText) | Some(InteractionKind::EditText) | None => {
                match key_code {
                    KeyCode::Enter if key.modifiers.is_empty() => {
                        self.submit_input();
                    }
                    KeyCode::Esc => {
                        self.input_mode = false;
                        self.input_textarea = MarkdownEdit::default();
                        self.choice_selected = 0;
                    }
                    KeyCode::PageUp if self.has_scrollable_document() => self.scroll_by(10),
                    KeyCode::PageDown if self.has_scrollable_document() => self.scroll_by(-10),
                    _ => {
                        let outcome = self.input_textarea.handle_key(&key);
                        self.remember_md_mode(outcome);
                    }
                }
            }
            _ => match key_code {
                KeyCode::Esc => {
                    self.input_mode = false;
                    self.input_textarea = MarkdownEdit::default();
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
                // longer than the pane while its approval prompt was open -
                // which is exactly when you need to read it.
                KeyCode::PageUp => self.scroll_by(10),
                KeyCode::PageDown => self.scroll_by(-10),
                KeyCode::Home => self.scroll_by(i32::MAX),
                KeyCode::End => self.scroll_by(i32::MIN),
                _ => {}
            },
        }
    }

    /// Enter/Space in the Context view (or a click on the row): fold or unfold
    /// the row under the tree cursor.
    pub(super) fn toggle_context_row(&mut self) {
        use super::context_tree::TreeRow;
        let rows = self.context_tree_rows();
        match rows.get(self.context_tree.cursor) {
            Some(TreeRow::RegionHeader { region }) => {
                let region = region.clone();
                if !self.context_tree.collapsed_regions.remove(&region) {
                    self.context_tree.collapsed_regions.insert(region);
                }
                // The cursor stays on the header it just folded; a stale
                // cursor from a shrunken snapshot resolves through
                // `rows.get()` returning `None` on the next keypress.
            }
            Some(TreeRow::EntryStub { region, index }) => {
                let key = (region.clone(), *index);
                if !self.context_tree.expanded_entries.remove(&key) {
                    self.context_tree.expanded_entries.insert(key);
                }
            }
            None => {}
        }
        self.context_tree.follow_cursor = true;
        // Kept per run, so reopening this one finds it the way it was left.
        self.save_ui_state();
    }

    /// Show stage tab `idx`, from a number key or a click on the tab. Out of
    /// range does nothing: the run has no such stage to show.
    pub(super) fn select_stage_tab(&mut self, idx: usize) {
        let max_stage = self
            .selected_agent()
            .map(|a| a.num_stages.saturating_sub(1))
            .unwrap_or(0);
        if idx > max_stage {
            return;
        }
        self.selected_stage = idx;
        self.detail_scroll = 0;
        self.review_scroll = 0;
        self.search_mode = false;
        self.search_query.clear();
        self.search_match_idx = 0;
        self.band_select_tab(idx);
    }

    /// Move the Context view's tree cursor, clamped, and scroll to it.
    fn move_context_cursor(&mut self, delta: isize) {
        let len = self.context_tree_rows().len();
        if len == 0 {
            return;
        }
        self.context_tree.cursor = self
            .context_tree
            .cursor
            .saturating_add_signed(delta)
            .min(len - 1);
        self.context_tree.follow_cursor = true;
    }

    /// Jump the tree cursor to the previous/next region header.
    fn jump_context_region(&mut self, forward: bool) {
        let rows = self.context_tree_rows();
        if let Some(target) =
            super::context_tree::nearest_region_row(&rows, self.context_tree.cursor, forward)
        {
            self.context_tree.cursor = target;
            self.context_tree.follow_cursor = true;
        }
    }

    fn handle_detail_view_key(&mut self, key_code: KeyCode) {
        if self.stage_explorer.is_some() {
            self.handle_explorer_key(key_code);
            return;
        }
        let in_context = self.stage_content_mode == StageContentMode::Context;
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
            // Stage tab navigation: through the graph band when it is on
            // screen (its selection is the tab), by index otherwise.
            KeyCode::Left | KeyCode::Right if self.band_shown() => self.band_key(key_code),
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
                self.select_stage_tab((c as usize) - ('1' as usize));
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
                // any active agent that accepts them - same key, same input area.
                if self.selected_stage_can_respond() || self.selected_agent_accepts_messages() {
                    self.input_mode = true;
                    self.choice_selected = 0;
                    self.seed_input_textarea();
                }
            }
            // In the Context view the up/down keys drive the tree cursor
            // (the view scrolls to follow it); elsewhere they scroll, through
            // `scroll_by` - the one place that decides which pane a gesture
            // moves.
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') if in_context => {
                let delta = if matches!(key_code, KeyCode::Up | KeyCode::Char('k')) {
                    -1
                } else {
                    1
                };
                self.move_context_cursor(delta);
            }
            KeyCode::Up | KeyCode::Char('k') => self.scroll_by(1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_by(-1),
            KeyCode::PageUp => self.scroll_by(10),
            KeyCode::PageDown => self.scroll_by(-10),
            // Fold / unfold the row under the Context tree's cursor.
            KeyCode::Enter | KeyCode::Char(' ') if in_context => self.toggle_context_row(),
            // Jump between region headers.
            KeyCode::Char('[') if in_context => self.jump_context_region(false),
            KeyCode::Char(']') if in_context => self.jump_context_region(true),
            // The full-screen stage explorer.
            KeyCode::Char('g') => {
                self.open_stage_explorer();
            }
            // Home/End are the documented jumps; b/e stay as the historical
            // aliases. (`detail_scroll` counts up from the bottom, so "top of
            // the document" is the maximum offset.)
            KeyCode::Char('b') | KeyCode::Home => {
                self.detail_scroll = usize::MAX;
                self.review_scroll = usize::MAX;
            }
            KeyCode::Char('e') | KeyCode::End => {
                self.detail_scroll = 0;
                self.review_scroll = 0;
            }
            KeyCode::Char('?') | KeyCode::F(1) => {
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
            // `x` kills, behind a real confirmation (`k` is unbound here: in a
            // view whose neighbors are l/o/c, a vim-reflex `k` killing the run
            // is exactly the accident this replaces).
            KeyCode::Char('x') => self.request_kill(),
            KeyCode::Char('p') => {
                self.handle_pause();
            }
            KeyCode::Char('r') => {
                self.handle_resume();
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

    /// Pause the selected agent (main list and detail view share this: unlike
    /// kill, there is no input state to reset). Gated on the states the daemon
    /// will actually pause, so an ineligible row sends nothing instead of
    /// producing a guaranteed refusal toast.
    fn handle_pause(&mut self) {
        // `Waiting` is included because the daemon pauses the whole sub-agent
        // tree: a fan-out parent sits here while its children do the work, so
        // refusing the key would leave the only row a user thinks to press it on
        // doing nothing at all.
        if let Some(agent) = self.selected_agent()
            && matches!(
                agent.status,
                AgentDisplayStatus::Active
                    | AgentDisplayStatus::Idle
                    | AgentDisplayStatus::Stale
                    | AgentDisplayStatus::Waiting
            )
        {
            let agent_id = agent.id.clone();
            // Snapshotted before the send, which is what the optimistic flip
            // below keys on.
            let waiting = agent.status == AgentDisplayStatus::Waiting;
            let _ = self.cmd_tx.send(DaemonCommand::Pause {
                run_id: agent_id.clone(),
            });
            // See the comment in `handle_kill_from_detail` - the index is still
            // valid because the `cmd_tx.send` above does not touch
            // `display_indices`/`agents`/`selected`.
            let idx = self
                .selected_agent_raw_idx()
                .expect("selected_agent() returned Some above");
            let a = self
                .agents
                .get_mut(idx)
                .expect("index snapshotted from the still-unchanged display_indices/agents above");
            // A waiting parent keeps its own status - the merge poll depends on
            // it, so the daemon pauses the children instead. Flipping this row
            // would be a claim the next poll contradicts; the children's rows
            // are where the pause shows up.
            if !waiting {
                a.status = AgentDisplayStatus::Paused;
            }
            self.add_log(format!("{}: pause requested", agent_id));
        }
    }

    /// Resume the selected agent if it is paused.
    fn handle_resume(&mut self) {
        // `Waiting` for the same reason as the pause key: that is the row a
        // fan-out parent occupies while its paused children wait to be let go.
        if let Some(agent) = self.selected_agent()
            && matches!(
                agent.status,
                AgentDisplayStatus::Paused | AgentDisplayStatus::Waiting
            )
        {
            let agent_id = agent.id.clone();
            let waiting = agent.status == AgentDisplayStatus::Waiting;
            let _ = self.cmd_tx.send(DaemonCommand::Resume {
                run_id: agent_id.clone(),
            });
            // See the comment in `handle_kill_from_detail` - the index is still
            // valid because the `cmd_tx.send` above does not touch
            // `display_indices`/`agents`/`selected`.
            let idx = self
                .selected_agent_raw_idx()
                .expect("selected_agent() returned Some above");
            let a = self
                .agents
                .get_mut(idx)
                .expect("index snapshotted from the still-unchanged display_indices/agents above");
            if !waiting {
                a.status = AgentDisplayStatus::Active;
            }
            self.add_log(format!("{}: resume requested", agent_id));
        }
    }

    fn handle_main_list_key(&mut self, key_code: KeyCode) {
        match key_code {
            // Esc dismisses: the filter first, then the marks. It does not
            // quit - `q` quits, the same as every other Leviath TUI.
            KeyCode::Esc => {
                if !self.list_search_query.is_empty() {
                    self.list_search_query.clear();
                    self.selected = 0;
                    self.update_display_indices();
                } else if !self.marked.is_empty() {
                    self.marked.clear();
                }
            }
            // Space marks or unmarks the selected run for a group kill or
            // delete, then moves down one row so repeated presses sweep the
            // list.
            KeyCode::Char(' ') => {
                if let Some(agent) = self.selected_agent() {
                    let id = agent.id.clone();
                    if !self.marked.remove(&id) {
                        self.marked.insert(id);
                    }
                    if self.selected < self.display_indices.len() - 1 {
                        self.selected += 1;
                        self.table_state.select(Some(self.selected));
                    }
                }
            }
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Up | KeyCode::Char('k') => {
                if !self.display_indices.is_empty() && self.selected > 0 {
                    self.selected -= 1;
                    self.table_state.select(Some(self.selected));
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.display_indices.is_empty()
                    && self.selected < self.display_indices.len() - 1
                {
                    self.selected += 1;
                    self.table_state.select(Some(self.selected));
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                if !self.display_indices.is_empty() {
                    self.selected = 0;
                    self.table_state.select(Some(0));
                }
            }
            KeyCode::End | KeyCode::Char('G') => {
                if !self.display_indices.is_empty() {
                    self.selected = self.display_indices.len() - 1;
                    self.table_state.select(Some(self.selected));
                }
            }
            // ← and → work the sub-agent tree. On a run with workers they
            // fold and unfold it; on a row that cannot fold, ← climbs to the
            // parent and → steps into the first child, so the arrows always
            // do something rather than dying on a leaf.
            KeyCode::Left => self.collapse_or_climb(),
            KeyCode::Right => self.expand_or_descend(),
            KeyCode::Tab | KeyCode::BackTab => {
                self.main_focus = MainPane::LogPane;
            }
            KeyCode::Enter => {
                if !self.display_indices.is_empty() {
                    self.open_detail_view();
                }
            }
            KeyCode::Char('/') => {
                self.list_search_mode = true;
                self.list_search_query.clear();
                self.selected = 0;
                self.update_display_indices();
            }
            KeyCode::Char('s') => self.cycle_sort_mode(),
            KeyCode::Char('d') => self.request_delete(),
            // `x` kills, behind a real confirmation. `k` deliberately does
            // not: it is list navigation here, like everywhere else.
            KeyCode::Char('x') => self.request_kill(),
            KeyCode::Char('p') => {
                self.handle_pause();
            }
            KeyCode::Char('r') => {
                self.handle_resume();
            }
            KeyCode::Char('?') | KeyCode::F(1) => {
                self.show_help = true;
            }
            KeyCode::Char('m') => {
                // Open the MCP management screen; load the current servers.
                self.mcp_screen = true;
                self.mcp_selected = 0;
                self.refresh_mcp_rows();
            }
            KeyCode::Char('n') => self.open_new_run_screen(),
            KeyCode::Char('a') => self.open_agents_screen(),
            _ => {}
        }
    }

    /// Fold or unfold the sub-agents of the run on row `pos`, and leave the
    /// highlight on that row. A row with no sub-agents has nothing to fold.
    pub(super) fn toggle_run_fold_at(&mut self, pos: usize) {
        let Some(id) = self
            .display_indices
            .get(pos)
            .and_then(|&i| self.agents.get(i))
            .map(|a| a.id.clone())
        else {
            return;
        };
        if !self.collapsed_runs.remove(&id) {
            // Only a run with sub-agents folds; folding a leaf would draw an
            // arrow on it that does nothing.
            if !self.tree_rows.get(pos).is_some_and(|r| r.expandable) {
                return;
            }
            self.collapsed_runs.insert(id.clone());
        }
        // Written on the keystroke rather than on the way out: a dashboard is
        // left running for days and closed by whatever closes the terminal, so
        // "save on quit" is a save that often never happens.
        self.save_ui_state();
        self.update_display_indices();
        // The run just folded is still on screen - it is the row the fold
        // happened on - so this lands back on it.
        let row = self.row_of_run(&id).unwrap_or(self.selected);
        self.selected = row;
        self.table_state.select(Some(row));
    }

    /// `←` on the run list: fold the selected run's sub-agents, or - when
    /// there is nothing to fold - move up to its parent.
    fn collapse_or_climb(&mut self) {
        let Some(row) = self.tree_rows.get(self.selected).cloned() else {
            return;
        };
        if row.expandable && !row.collapsed {
            self.toggle_run_fold_at(self.selected);
            return;
        }
        // The parent is the nearest row above with a shorter prefix: rows are
        // in depth-first order, so that is the run this one hangs under.
        let width = row.prefix.chars().count();
        if let Some(parent) = self.tree_rows[..self.selected]
            .iter()
            .rposition(|r| r.prefix.chars().count() < width)
        {
            self.selected = parent;
            self.table_state.select(Some(parent));
        }
    }

    /// `→` on the run list: unfold the selected run's sub-agents, or - when
    /// they are already showing - step down into the first of them.
    fn expand_or_descend(&mut self) {
        let Some(row) = self.tree_rows.get(self.selected).cloned() else {
            return;
        };
        if !row.expandable {
            return;
        }
        if row.collapsed {
            self.toggle_run_fold_at(self.selected);
            return;
        }
        // Depth-first order puts the first child immediately below, and an
        // unfolded run with sub-agents always has one; the clamp is only so a
        // stale row index cannot point past the list.
        let child = (self.selected + 1).min(self.display_indices.len().saturating_sub(1));
        self.selected = child;
        self.table_state.select(Some(child));
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
            // Esc closes the screen (back); `q` quits the app, the same as
            // everywhere else.
            KeyCode::Esc => {
                self.mcp_screen = false;
            }
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') | KeyCode::F(1) => self.show_help = true,
            KeyCode::Up | KeyCode::Char('k') => {
                self.mcp_selected = self.mcp_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.mcp_rows.is_empty() && self.mcp_selected + 1 < self.mcp_rows.len() {
                    self.mcp_selected += 1;
                }
            }
            KeyCode::Char('a') => {
                self.mcp_add_mode = true;
                self.mcp_add_input.clear();
            }
            KeyCode::Char('d') => {
                self.mcp_request_remove();
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
                    // Preserve indentation / internal newlines - only trim the
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
                    // The index-to-scope mapping lives with the labels, so the
                    // two cannot drift; anything it does not recognise denies.
                    let (approved, scope) = match leviath_core::interaction::approval_choice(idx) {
                        Some(scope) => (true, scope),
                        None => (false, ApprovalScope::Once),
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
        self.input_textarea = MarkdownEdit::default();
        self.choice_selected = 0;

        let answered_id = resp.request_id.clone();
        // The index is still valid because nothing since the
        // `self.selected_agent()` lookup at the top of this function (which is
        // where `req`/`agent_id` came from) touches
        // `display_indices`/`agents`/`selected` - an `if let` guard here would
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
            broken_scripts: Vec::new(),
            waiting_prompt: None,
            wait_reason: None,
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
            last_progress_at: None,
            active_until: None,
            waiting_secs: 0,
            graph: None,
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
    fn q_quits_from_the_main_list_and_esc_does_not() {
        let mut dash = make_test_dashboard();
        dash.handle_key(key(KeyCode::Esc));
        assert!(!dash.should_quit, "Esc dismisses; it never quits");
        dash.handle_key(key(KeyCode::Char('q')));
        assert!(dash.should_quit);
    }

    #[test]
    fn ctrl_c_quits_from_anywhere() {
        for setup in [false, true] {
            let mut dash = make_test_dashboard();
            dash.detail_view = setup;
            dash.handle_key(crossterm::event::KeyEvent::new(
                KeyCode::Char('c'),
                crossterm::event::KeyModifiers::CONTROL,
            ));
            assert!(dash.should_quit, "detail={setup}");
        }
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
    fn help_overlay_dismissed_only_deliberately() {
        let mut dash = make_test_dashboard();
        dash.show_help = true;
        // A stray key is ignored: it neither closes help nor acts underneath.
        dash.handle_key(key(KeyCode::Char('x')));
        assert!(dash.show_help);
        assert!(dash.pending_confirm.is_none(), "x did not open a dialog");
        dash.handle_key(key(KeyCode::Esc));
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
    fn the_delete_dialog_confirms_with_y_and_deletes() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-del", AgentDisplayStatus::Complete));
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Char('d')));
        assert!(dash.pending_confirm.is_some(), "d opens the dialog");

        dash.handle_key(key(KeyCode::Char('y')));
        assert!(dash.pending_confirm.is_none());
        assert!(
            dash.agents.is_empty(),
            "confirming removes the run from the list"
        );
    }

    #[test]
    fn the_delete_dialog_ignores_stray_keys_and_enter_declines() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-keep", AgentDisplayStatus::Complete));
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Char('d')));

        // A stray key neither confirms nor dismisses.
        dash.handle_key(key(KeyCode::Char('z')));
        assert!(dash.pending_confirm.is_some());

        // Enter activates the focused button, which defaults to No.
        dash.handle_key(key(KeyCode::Enter));
        assert!(dash.pending_confirm.is_none());
        assert_eq!(dash.agents.len(), 1, "declining deletes nothing");
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

    /// Up/Down move the choice selection, so PageUp/PageDown/Home/End must
    /// scroll the document - without them a plan longer than the pane cannot
    /// be read while its approval prompt is open, which is the only time it is
    /// shown.
    #[test]
    fn a_plan_can_be_scrolled_while_its_approval_prompt_is_open() {
        let mut dash = dashboard_awaiting_a_plan();
        assert_eq!(dash.detail_scroll, 0);

        // In input mode the plan is drawn in the content pane - the separate
        // review pane is suppressed - so that is what has to move. Verified
        // against the real dashboard through a pty: the pane's own position
        // readout went 100% → 91% → 81% on two PageUps.
        dash.handle_key(key(KeyCode::PageUp));
        assert_eq!(dash.detail_scroll, 10, "PageUp scrolls the plan");
        assert_eq!(dash.review_scroll, 0, "and not a pane nobody can see");
        dash.handle_key(key(KeyCode::PageDown));
        assert_eq!(dash.detail_scroll, 0, "and PageDown comes back");

        // Down still selects rather than scrolling - the plan keys were added
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
        // is what is rendered - and so what the wheel moves.
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
        // While editing, the textarea owns the pane - no separate review body.
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

        dash.input_textarea.area_mut().insert_str("stale");
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
            MarkdownEdit::new(vec!["  indented".to_string(), "second line".to_string()]);
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
        dash.input_textarea = MarkdownEdit::new(vec!["   ".to_string()]);
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
        // kind resolves to None when there's no pending_request at all -
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

    // ─── handle_pause / handle_resume ─────────────────────────────────────

    /// Every state the daemon will pause is pausable from the list, sends the
    /// command, and flips the row optimistically.
    #[test]
    fn pause_sends_command_for_each_pausable_state() {
        for status in [
            AgentDisplayStatus::Active,
            AgentDisplayStatus::Idle,
            AgentDisplayStatus::Stale,
        ] {
            let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
            let mut dash = Dashboard::new(cmd_tx);
            dash.agents.push(make_test_agent("run-1", status.clone()));
            dash.update_display_indices();
            dash.handle_key(key(KeyCode::Char('p')));
            assert_eq!(
                cmd_rx.try_recv().ok(),
                Some(DaemonCommand::Pause {
                    run_id: "run-1".to_string()
                }),
                "pause from {status:?} sends the command"
            );
            assert_eq!(dash.agents[0].status, AgentDisplayStatus::Paused);
        }
    }

    /// Finished runs are not pausable: nothing is sent, so the user never sees a
    /// guaranteed refusal toast. `Waiting` is deliberately absent - see
    /// `pause_and_resume_reach_a_waiting_parents_children`.
    #[test]
    fn pause_is_a_no_op_for_unpausable_states() {
        for status in [
            AgentDisplayStatus::Paused,
            AgentDisplayStatus::Complete,
            AgentDisplayStatus::Cancelled,
        ] {
            let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
            let mut dash = Dashboard::new(cmd_tx);
            dash.agents.push(make_test_agent("run-1", status.clone()));
            dash.update_display_indices();
            dash.handle_key(key(KeyCode::Char('p')));
            assert!(
                cmd_rx.try_recv().is_err(),
                "pause from {status:?} sends nothing"
            );
            assert_eq!(dash.agents[0].status, status);
        }
    }

    #[test]
    fn resume_sends_command_for_a_paused_run_only() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Paused));
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Char('r')));
        assert_eq!(
            cmd_rx.try_recv().ok(),
            Some(DaemonCommand::Resume {
                run_id: "run-1".to_string()
            })
        );
        assert_eq!(dash.agents[0].status, AgentDisplayStatus::Active);
    }

    /// A fan-out parent sits at `Waiting` while its children do the work, and it
    /// is the row a user actually presses `p` on. The daemon pauses the whole
    /// sub-agent tree, so the key has to reach it - but the parent keeps its own
    /// status, because the merge poll reads it, so this row must not flip.
    /// Claiming otherwise would be undone by the very next poll.
    #[test]
    fn pause_and_resume_reach_a_waiting_parents_children() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        dash.agents
            .push(make_test_agent("parent", AgentDisplayStatus::Waiting));
        dash.update_display_indices();

        dash.handle_key(key(KeyCode::Char('p')));
        assert_eq!(
            cmd_rx.try_recv().ok(),
            Some(DaemonCommand::Pause {
                run_id: "parent".to_string()
            }),
            "the request has to get through; the children are what pauses"
        );
        assert_eq!(
            dash.agents[0].status,
            AgentDisplayStatus::Waiting,
            "the parent's own row does not lie about a status it keeps"
        );

        dash.handle_key(key(KeyCode::Char('r')));
        assert_eq!(
            cmd_rx.try_recv().ok(),
            Some(DaemonCommand::Resume {
                run_id: "parent".to_string()
            }),
            "and resuming through the parent is how the children are let go"
        );
        assert_eq!(dash.agents[0].status, AgentDisplayStatus::Waiting);
    }

    #[test]
    fn resume_is_a_no_op_for_a_run_that_is_not_paused() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Char('r')));
        assert!(cmd_rx.try_recv().is_err());
        assert_eq!(dash.agents[0].status, AgentDisplayStatus::Active);
    }

    /// With no agents at all, both keys fall through the `selected_agent()`
    /// guard without panicking or sending.
    #[test]
    fn pause_and_resume_with_no_agents_do_nothing() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        dash.handle_key(key(KeyCode::Char('p')));
        dash.handle_key(key(KeyCode::Char('r')));
        assert!(cmd_rx.try_recv().is_err());
    }

    /// The detail view shares the same handlers via its own `p`/`r` bindings.
    #[test]
    fn pause_and_resume_work_from_the_detail_view() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;

        dash.handle_key(key(KeyCode::Char('p')));
        assert_eq!(
            cmd_rx.try_recv().ok(),
            Some(DaemonCommand::Pause {
                run_id: "run-1".to_string()
            })
        );
        assert_eq!(dash.agents[0].status, AgentDisplayStatus::Paused);

        dash.handle_key(key(KeyCode::Char('r')));
        assert_eq!(
            cmd_rx.try_recv().ok(),
            Some(DaemonCommand::Resume {
                run_id: "run-1".to_string()
            })
        );
        assert_eq!(dash.agents[0].status, AgentDisplayStatus::Active);
    }

    // ─── kill via x + confirmation ────────────────────────────────────────

    // ─── ← / → on the run list: the sub-agent tree ───────────────────────

    /// A parent with two sub-agents, selected on its own row.
    fn dash_with_a_subtree() -> Dashboard {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("parent", AgentDisplayStatus::Active));
        for id in ["worker-a", "worker-b"] {
            let mut child = make_test_agent(id, AgentDisplayStatus::Active);
            child.parent_id = Some("parent".to_string());
            dash.agents.push(child);
        }
        dash.update_display_indices();
        dash.selected = dash.row_of_run("parent").expect("the parent is a row");
        dash
    }

    #[test]
    fn left_and_right_fold_and_unfold_a_run_s_sub_agents() {
        let mut dash = dash_with_a_subtree();
        assert_eq!(dash.display_indices.len(), 3);

        dash.handle_key(key(KeyCode::Left));
        assert_eq!(dash.display_indices.len(), 1, "the workers folded away");
        assert!(dash.tree_rows[0].collapsed);
        assert_eq!(dash.selected, 0, "the highlight stayed on the parent");

        dash.handle_key(key(KeyCode::Right));
        assert_eq!(dash.display_indices.len(), 3, "and unfolded again");
        assert!(!dash.tree_rows[0].collapsed);
        assert_eq!(dash.selected, 0);
    }

    /// With nothing left to fold the arrows walk the tree instead of dying:
    /// → steps into the first worker, ← climbs back to the parent.
    #[test]
    fn the_arrows_walk_the_tree_when_there_is_nothing_to_fold() {
        let mut dash = dash_with_a_subtree();
        dash.handle_key(key(KeyCode::Right));
        assert_eq!(
            dash.selected_agent().map(|a| a.id.as_str()),
            Some("worker-a"),
            "an unfolded parent steps down to its first worker"
        );
        // A worker has no sub-agents of its own, so → does nothing at all…
        dash.handle_key(key(KeyCode::Right));
        assert_eq!(
            dash.selected_agent().map(|a| a.id.as_str()),
            Some("worker-a")
        );
        // …and ← climbs out of the subtree.
        dash.handle_key(key(KeyCode::Left));
        assert_eq!(dash.selected_agent().map(|a| a.id.as_str()), Some("parent"));
    }

    /// ← on a run that is nobody's child, and the arrows on an empty list,
    /// both leave the selection where it is rather than panicking.
    #[test]
    fn the_arrows_are_harmless_with_no_tree_to_walk() {
        let mut dash = make_test_dashboard();
        dash.handle_key(key(KeyCode::Left));
        dash.handle_key(key(KeyCode::Right));
        assert_eq!(dash.selected, 0);

        dash.agents
            .push(make_test_agent("lonely", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Left));
        assert_eq!(dash.selected, 0, "a root leaf has no parent to climb to");
        assert!(dash.collapsed_runs.is_empty(), "and nothing folded");
    }

    /// The fold reaches disk on the keystroke that makes it, and the unfold
    /// reaches disk too - a store that only ever grew would re-fold a run the
    /// user had deliberately opened.
    #[test]
    fn folding_and_unfolding_both_reach_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui-state.json");
        let mut dash = dash_with_a_subtree();
        dash.ui_state_path = Some(path.clone());

        dash.handle_key(key(KeyCode::Left));
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("parent"),
            "the fold was written without waiting for a quit"
        );

        dash.handle_key(key(KeyCode::Right));
        assert!(
            !std::fs::read_to_string(&path).unwrap().contains("parent"),
            "and so was the unfold"
        );
    }

    /// Folding is keyed by run, so a row index that no longer names one (or a
    /// run with no sub-agents) is a no-op rather than a phantom fold.
    #[test]
    fn folding_an_unfoldable_row_changes_nothing() {
        let mut dash = dash_with_a_subtree();
        dash.toggle_run_fold_at(99);
        assert!(dash.collapsed_runs.is_empty(), "no such row");
        let worker_row = dash.row_of_run("worker-a").unwrap();
        dash.toggle_run_fold_at(worker_row);
        assert!(
            dash.collapsed_runs.is_empty(),
            "a worker has nothing to fold"
        );
    }

    /// `c` no longer cancels from the list (it was an unconfirmed kill by
    /// another name); it is simply unbound there.
    #[test]
    fn c_in_the_main_list_does_nothing() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Char('c')));
        assert_eq!(dash.agents[0].status, AgentDisplayStatus::Active);
        assert!(cmd_rx.try_recv().is_err());
    }

    /// `k` navigates; it must never kill. The kill key is `x`, behind a
    /// confirmation.
    #[test]
    fn k_in_the_main_list_moves_the_cursor_not_the_daemon() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.agents
            .push(make_test_agent("run-2", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.selected = 1;
        dash.table_state.select(Some(1));
        dash.handle_key(key(KeyCode::Char('k')));
        assert_eq!(dash.selected, 0, "k moves up");
        assert_eq!(dash.agents[0].status, AgentDisplayStatus::Active);
        assert!(cmd_rx.try_recv().is_err(), "nothing was killed");
    }

    /// Every state that is not a finished one can be killed. A gate of only
    /// `Active | Waiting` would make a run showing IDLE or STALE - exactly the
    /// states a user reaches for the kill key in - unkillable.
    #[test]
    fn every_unfinished_state_can_be_killed_after_confirming() {
        for status in [
            AgentDisplayStatus::Active,
            AgentDisplayStatus::Waiting,
            AgentDisplayStatus::Idle,
            AgentDisplayStatus::Stale,
        ] {
            let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
            let mut dash = Dashboard::new(cmd_tx);
            dash.agents.push(make_test_agent("run-1", status.clone()));
            dash.update_display_indices();
            dash.handle_key(key(KeyCode::Char('x')));
            assert!(
                dash.pending_confirm.is_some(),
                "{status:?} must open the kill dialog"
            );
            dash.handle_key(key(KeyCode::Char('y')));

            assert_eq!(
                cmd_rx.try_recv().unwrap(),
                DaemonCommand::Cancel {
                    run_id: "run-1".to_string()
                },
                "{status:?} must be killable via x + confirm"
            );
        }
    }

    /// …and a finished run is left alone: `x` does not even open the dialog,
    /// so there is nothing to mis-confirm.
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
            dash.handle_key(key(KeyCode::Char('x')));

            assert!(dash.pending_confirm.is_none(), "{status:?}: no dialog");
            assert!(
                cmd_rx.try_recv().is_err(),
                "{status:?} must not be re-killed"
            );
        }
    }

    /// Declining the kill dialog leaves the run exactly as it was.
    #[test]
    fn declining_the_kill_dialog_kills_nothing() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Char('x')));
        dash.handle_key(key(KeyCode::Esc));

        assert!(dash.pending_confirm.is_none());
        assert_eq!(dash.agents[0].status, AgentDisplayStatus::Active);
        assert!(cmd_rx.try_recv().is_err());
    }

    #[test]
    fn confirmed_kill_clears_local_waiting_state_and_cancels() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
        agent.waiting_prompt = Some("?".to_string());
        agent.pending_request = Some(interaction::InteractionRequest::free_text(
            "q1", "?", "main", true,
        ));
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Char('x')));
        dash.handle_key(key(KeyCode::Char('y')));

        assert_cancelled(&dash.agents[0].status);
        assert!(dash.agents[0].pending_request.is_none());
        assert!(dash.agents[0].waiting_prompt.is_none());
        assert_eq!(
            cmd_rx.try_recv().unwrap(),
            DaemonCommand::Cancel {
                run_id: "run-1".to_string()
            }
        );
    }

    #[test]
    fn main_list_d_no_agent_selected_is_noop() {
        let mut dash = make_test_dashboard();
        dash.handle_key(key(KeyCode::Char('d')));
        assert!(dash.pending_confirm.is_none());
        assert!(dash.agents.is_empty());
    }

    #[test]
    fn x_with_no_agent_selected_is_a_noop() {
        let mut dash = make_test_dashboard();
        dash.handle_key(key(KeyCode::Char('x')));
        assert!(dash.pending_confirm.is_none());

        dash.detail_view = true;
        dash.handle_key(key(KeyCode::Char('x')));
        assert!(dash.pending_confirm.is_none());
    }

    #[test]
    fn tab_focuses_the_log_pane_and_its_keys_scroll() {
        let mut dash = make_test_dashboard();
        // The seeded log tail is shared test state; a known length makes the
        // scroll arithmetic below deterministic.
        dash.log.clear();
        for i in 0..50 {
            dash.log.push(LogEntry {
                timestamp: "12:00:00".to_string(),
                message: format!("line {i}"),
            });
        }
        dash.log_viewport = 10;
        dash.handle_key(key(KeyCode::Tab));
        assert_eq!(dash.main_focus, MainPane::LogPane);

        dash.handle_key(key(KeyCode::Up));
        dash.handle_key(key(KeyCode::Char('k')));
        assert_eq!(dash.log_scroll.offset_from_tail, 2);
        dash.handle_key(key(KeyCode::PageUp));
        assert_eq!(dash.log_scroll.offset_from_tail, 12);
        dash.handle_key(key(KeyCode::Down));
        dash.handle_key(key(KeyCode::Char('j')));
        dash.handle_key(key(KeyCode::PageDown));
        assert_eq!(dash.log_scroll.offset_from_tail, 0);

        dash.handle_key(key(KeyCode::Home));
        assert_eq!(dash.log_scroll.offset_from_tail, 40);
        dash.handle_key(key(KeyCode::End));
        assert!(dash.log_scroll.is_tailing());
        dash.handle_key(key(KeyCode::Char('g')));
        assert!(!dash.log_scroll.is_tailing());
        dash.handle_key(key(KeyCode::Char('G')));
        assert!(dash.log_scroll.is_tailing());

        // ? opens help from the log pane; a stray key is ignored.
        dash.handle_key(key(KeyCode::Char('?')));
        assert!(dash.show_help);
        dash.handle_key(key(KeyCode::Esc));
        assert!(!dash.show_help);
        dash.handle_key(key(KeyCode::Char('z')));

        // Esc hands focus back to the list; q quits from the pane.
        dash.handle_key(key(KeyCode::Esc));
        assert_eq!(dash.main_focus, MainPane::RunList);
        dash.handle_key(key(KeyCode::Tab));
        dash.handle_key(key(KeyCode::Char('q')));
        assert!(dash.should_quit);
    }

    #[test]
    fn home_end_and_enter_navigate_the_main_list() {
        let mut dash = make_test_dashboard();
        let mut a = make_test_agent("run-1", AgentDisplayStatus::Active);
        a.stage_index = 1;
        a.num_stages = 3;
        dash.agents.push(a);
        dash.agents
            .push(make_test_agent("run-2", AgentDisplayStatus::Active));
        dash.agents
            .push(make_test_agent("run-3", AgentDisplayStatus::Active));
        dash.update_display_indices();

        dash.handle_key(key(KeyCode::End));
        assert_eq!(dash.selected, 2);
        dash.handle_key(key(KeyCode::Home));
        assert_eq!(dash.selected, 0);
        dash.handle_key(key(KeyCode::Char('G')));
        assert_eq!(dash.selected, 2);
        dash.handle_key(key(KeyCode::Char('g')));
        assert_eq!(dash.selected, 0);

        // Enter opens the detail view at the selected run's current stage.
        dash.handle_key(key(KeyCode::Enter));
        assert!(dash.detail_view);
        assert_eq!(dash.selected_stage, 1);

        // With no rows at all, the same keys are no-ops.
        let mut empty = make_test_dashboard();
        for code in [KeyCode::Home, KeyCode::End, KeyCode::Enter] {
            empty.handle_key(key(code));
        }
        assert!(!empty.detail_view);
    }

    #[test]
    fn s_cycles_the_sort_mode_from_the_keyboard() {
        let mut dash = make_test_dashboard();
        assert_eq!(dash.sort_mode, SortMode::StartedAt);
        dash.handle_key(key(KeyCode::Char('s')));
        assert_eq!(dash.sort_mode, SortMode::RecentActivity);
    }

    #[test]
    fn the_confirm_handler_declines_to_act_without_a_dialog() {
        let mut dash = make_test_dashboard();
        dash.handle_confirm_key(&crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            crossterm::event::KeyModifiers::empty(),
        ));
        assert!(dash.pending_confirm.is_none());
        assert!(!dash.should_quit);
    }

    #[test]
    fn question_mark_opens_help_on_the_mcp_screen() {
        let mut dash = make_test_dashboard();
        dash.mcp_screen = true;
        dash.handle_key(key(KeyCode::Char('?')));
        assert!(dash.show_help);
    }

    fn context_agent(id: &str) -> DashboardAgent {
        let mut agent = make_test_agent(id, AgentDisplayStatus::Active);
        let entry = |content: &str| leviath_core::run_meta::RegionEntrySnapshot {
            content: content.to_string(),
            tokens: 5,
            kind: Default::default(),
            metadata: None,
            key: None,
            taint: Default::default(),
        };
        agent.context_snapshot = Some(std::sync::Arc::new(crate::runstate::ContextSnapshot {
            stage_name: "main".to_string(),
            total_tokens: 20,
            max_tokens: 100,
            regions: vec![
                crate::runstate::RegionSnapshot {
                    name: "system".to_string(),
                    kind: "pinned".to_string(),
                    current_tokens: 10,
                    max_tokens: 50,
                    entries: vec![entry("alpha"), entry("beta")],
                    description: None,
                },
                crate::runstate::RegionSnapshot {
                    name: "conversation".to_string(),
                    kind: "sliding".to_string(),
                    current_tokens: 10,
                    max_tokens: 50,
                    entries: vec![entry("gamma")],
                    description: None,
                },
            ],
        }));
        agent
    }

    #[test]
    fn the_context_tree_cursor_folds_and_jumps() {
        use crate::commands::dashboard::context_tree::TreeRow;
        let mut dash = make_test_dashboard();
        dash.agents.push(context_agent("run-ctx"));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.handle_key(key(KeyCode::Char('c')));
        assert_eq!(dash.stage_content_mode, StageContentMode::Context);

        // 5 rows: system, its 2 stubs, conversation, its stub.
        assert_eq!(dash.context_tree_rows().len(), 5);

        // j/k move the cursor and clamp.
        dash.handle_key(key(KeyCode::Char('j')));
        assert_eq!(dash.context_tree.cursor, 1);
        dash.handle_key(key(KeyCode::Up));
        dash.handle_key(key(KeyCode::Char('k')));
        assert_eq!(dash.context_tree.cursor, 0);

        // ] and [ jump between region headers.
        dash.handle_key(key(KeyCode::Char(']')));
        assert_eq!(
            dash.context_tree_rows()[dash.context_tree.cursor],
            TreeRow::RegionHeader {
                region: "conversation".to_string()
            }
        );
        assert_eq!(dash.context_tree.cursor, 3);
        dash.handle_key(key(KeyCode::Char('[')));
        assert_eq!(dash.context_tree.cursor, 0);

        // Space on an entry stub expands it; again collapses.
        dash.handle_key(key(KeyCode::Char('j')));
        dash.handle_key(key(KeyCode::Char(' ')));
        assert!(
            dash.context_tree
                .expanded_entries
                .contains(&("system".to_string(), 0))
        );
        dash.handle_key(key(KeyCode::Enter));
        assert!(dash.context_tree.expanded_entries.is_empty());

        // Enter on a header folds the region: its stubs leave the row list.
        dash.handle_key(key(KeyCode::Char('k')));
        dash.handle_key(key(KeyCode::Enter));
        assert!(dash.context_tree.collapsed_regions.contains("system"));
        assert_eq!(dash.context_tree_rows().len(), 3);
        // …and unfolds.
        dash.handle_key(key(KeyCode::Enter));
        assert_eq!(dash.context_tree_rows().len(), 5);

        // A cursor stranded past the end after a fold gets clamped.
        dash.handle_key(key(KeyCode::Char(']')));
        dash.handle_key(key(KeyCode::Char('j')));
        assert_eq!(dash.context_tree.cursor, 4);
        dash.handle_key(key(KeyCode::Char('[')));
        dash.handle_key(key(KeyCode::Enter)); // fold conversation? no - cursor on header 0?
    }

    #[test]
    fn context_keys_are_inert_outside_context_mode_and_without_rows() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-plain", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;

        // Output mode: j/k scroll, Enter/Space/brackets do nothing.
        dash.handle_key(key(KeyCode::Char('k')));
        assert_eq!(dash.detail_scroll, 1);
        dash.handle_key(key(KeyCode::Char('j')));
        assert_eq!(dash.detail_scroll, 0, "j scrolls back down outside context");
        dash.handle_key(key(KeyCode::Up));
        dash.handle_key(key(KeyCode::Down));
        assert_eq!(dash.detail_scroll, 0);
        dash.handle_key(key(KeyCode::Enter));
        dash.handle_key(key(KeyCode::Char(' ')));
        assert!(dash.context_tree.expanded_entries.is_empty());

        // Context mode with no snapshot: cursor keys are safe no-ops.
        dash.handle_key(key(KeyCode::Char('c')));
        dash.handle_key(key(KeyCode::Char('j')));
        dash.handle_key(key(KeyCode::Enter));
        dash.handle_key(key(KeyCode::Char('[')));
        dash.handle_key(key(KeyCode::Char(']')));
        assert_eq!(dash.context_tree.cursor, 0);
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
        // No agents at all - selected_agent() is None.
        dash.submit_input();
        assert!(dash.agents.is_empty());
    }

    // ─── kill from the detail view ────────────────────────────────────────

    #[test]
    fn kill_from_detail_active_agent() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.handle_key(key(KeyCode::Char('x')));
        dash.handle_key(key(KeyCode::Char('y')));
        assert_cancelled(&dash.agents[0].status);
    }

    #[test]
    fn kill_from_detail_complete_agent_no_op() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Complete));
        dash.update_display_indices();
        dash.detail_view = true;
        dash.handle_key(key(KeyCode::Char('x')));
        assert!(dash.pending_confirm.is_none());
        assert_complete(&dash.agents[0].status);
    }

    #[test]
    fn confirmed_kill_from_detail_resets_input_state() {
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
        dash.handle_key(key(KeyCode::Char('x')));
        dash.handle_key(key(KeyCode::Char('y')));

        assert_cancelled(&dash.agents[0].status);
        assert!(dash.agents[0].pending_request.is_none());
        assert!(!dash.input_mode);
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
        assert!(dash.pending_confirm.is_some());
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
        // is what a scroll must move. Asserting `review_scroll` here - a pane
        // that is not on screen - is precisely how a plan sits still while
        // every scroll key "works" and this test stays green.
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
                &[],
            ),
        );
        agent.waiting_prompt = Some("Allow tool?".to_string());
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;
        dash.choice_selected = 0;

        // Down through once / stage / run / deny
        for expected in 1..=3 {
            dash.handle_key(key(KeyCode::Down));
            assert_eq!(dash.choice_selected, expected);
        }
        // Can't go past last
        dash.handle_key(key(KeyCode::Down));
        assert_eq!(dash.choice_selected, 3);

        // Up back
        dash.handle_key(key(KeyCode::Up));
        assert_eq!(dash.choice_selected, 2);
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
                &[],
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
        dash.input_textarea.area_mut().insert_str("my answer");

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

        dash.input_textarea.area_mut().insert_str("/quit");

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

    /// Every option position, end to end: the index the user landed on has to
    /// become the scope the daemon is told about, or a person choosing "for this
    /// stage" silently gets something else.
    #[test]
    fn submit_input_tool_approval_sends_the_scope_the_option_named() {
        let cases = [
            (0, true, interaction::ApprovalScope::Once),
            (1, true, interaction::ApprovalScope::Stage),
            (2, true, interaction::ApprovalScope::Run),
            (3, false, interaction::ApprovalScope::Once),
        ];
        for (index, approved, scope) in cases {
            let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
            let mut dash = Dashboard::new(cmd_tx);
            let mut agent = make_test_agent("run-1", AgentDisplayStatus::Waiting);
            agent.pending_request = Some(
                leviath_core::interaction::InteractionRequest::tool_approval(
                    "ta1",
                    "bash",
                    serde_json::json!({"cmd": "ls"}),
                    "main",
                    &["shell:ls".to_string()],
                ),
            );
            agent.waiting_prompt = Some("Allow tool?".to_string());
            dash.agents.push(agent);
            dash.update_display_indices();
            dash.detail_view = true;
            dash.input_mode = true;
            dash.choice_selected = index;

            dash.submit_input();
            assert!(!dash.input_mode);
            assert!(dash.agents[0].pending_request.is_none());
            let cmd = cmd_rx.try_recv().expect("an Answer command was queued");
            assert_eq!(
                cmd,
                DaemonCommand::Answer {
                    response: interaction::InteractionResponse::approval("ta1", approved, scope),
                },
                "option {index}"
            );
        }
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

        dash.input_textarea
            .area_mut()
            .insert_str("in-process answer");

        dash.submit_input();

        assert!(!dash.input_mode);
        // Should have sent EngineCommand::SendInput
        let cmd = cmd_rx.try_recv();
        assert!(cmd.is_ok());
    }

    /// The response box is the shared long-form editor too, so its formatting
    /// chords have to survive the detail view's key routing.
    #[test]
    fn formatting_chords_reach_the_response_box() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.pending_request = None;
        agent.waiting_prompt = None;
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;

        dash.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('b'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        for c in "urgent".chars() {
            dash.handle_key(crossterm::event::KeyEvent::new(
                KeyCode::Char(c),
                crossterm::event::KeyModifiers::NONE,
            ));
        }
        assert_eq!(dash.input_textarea.text(), "**urgent**");
    }

    /// The response box's popup outranks the detail view's keys, so Enter
    /// finishes the link rather than sending the answer.
    #[test]
    fn the_response_boxs_popup_outranks_the_detail_views_keys() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.pending_request = None;
        agent.waiting_prompt = None;
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;
        dash.input_mode = true;

        dash.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('k'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert!(dash.input_textarea.is_modal());
        dash.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(dash.input_mode, "Enter did not send the answer");
        dash.handle_key(crossterm::event::KeyEvent::new(
            KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(!dash.input_textarea.is_modal(), "Esc closed the popup");
        assert!(dash.input_mode, "and not the box");
        assert!(cmd_rx.try_recv().is_err(), "nothing was sent");
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

        dash.input_textarea.area_mut().insert_str("hello mid-run");

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

        dash.input_textarea.area_mut().insert_str("/exit");
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

        dash.handle_key(key(KeyCode::Char('x')));
        dash.handle_key(key(KeyCode::Char('y')));

        assert_eq!(dash.agents[0].status, AgentDisplayStatus::Cancelled);

        // Should have sent CancelAgent command
        let cmd = cmd_rx.try_recv();
        assert!(cmd.is_ok());
    }

    // ─── x + confirm from the list for in-process agent ───────────────────

    #[test]
    fn kill_from_list_in_process_agent_sends_command() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        let agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        dash.agents.push(agent);
        dash.update_display_indices();

        dash.handle_key(key(KeyCode::Char('x')));
        dash.handle_key(key(KeyCode::Char('y')));

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

        dash.input_textarea.area_mut().insert_str("answer");

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

        dash.input_textarea.area_mut().insert_str("answer");
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
        // search_mode must be FALSE - in search mode, Char('3') is treated as a
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
        let points = vec![leviath_core::run_archive::RunPoint {
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
        dash.history = Some(crate::commands::dashboard::history::RunHistoryCache {
            run_id: "run-1".to_string(),
            visits: crate::commands::dashboard::history::derive_visits(&points),
            points,
            loaded_at_tick: u64::MAX,
        });
        dash.context_history_idx = Some(0);
        dash
    }

    #[test]
    fn detail_view_comma_and_period_route_to_history_step() {
        // With no archive on disk, stepping is a no-op - this exercises the
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
    }

    #[test]
    fn detail_view_esc_resets_context_history() {
        let mut dash = browsing_detail_dashboard();
        dash.handle_key(key(KeyCode::Esc));
        assert!(!dash.detail_view);
        assert_eq!(dash.context_history_idx, None);
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

    // ─── main list: Down key - three agents to confirm can-move path ──────

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

        // At bottom - cannot go further
        dash.handle_key(key(KeyCode::Down));
        assert_eq!(dash.selected, 2);
    }

    // ─── confirmed kill clears pending_request (list and detail) ──────────

    #[test]
    fn kill_from_list_run_state_agent_clears_pending_request() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::free_text(
            "r1", "?", "main", true,
        ));
        dash.agents.push(agent);
        dash.update_display_indices();

        dash.handle_key(key(KeyCode::Char('x')));
        dash.handle_key(key(KeyCode::Char('y')));

        assert!(dash.agents[0].pending_request.is_none());
        assert_cancelled(&dash.agents[0].status);
    }

    #[test]
    fn kill_from_detail_run_state_agent_clears_pending_request() {
        let mut dash = make_test_dashboard();
        let mut agent = make_test_agent("run-1", AgentDisplayStatus::Active);
        agent.pending_request = Some(leviath_core::interaction::InteractionRequest::free_text(
            "r1", "?", "main", true,
        ));
        dash.agents.push(agent);
        dash.update_display_indices();
        dash.detail_view = true;

        dash.handle_key(key(KeyCode::Char('x')));
        dash.handle_key(key(KeyCode::Char('y')));

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
    fn esc_closes_the_mcp_screen_and_q_quits_the_app() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = mcp_dash(dir.path());
        dash.mcp_screen = true;
        dash.handle_key(key(KeyCode::Esc));
        assert!(!dash.mcp_screen);
        assert!(!dash.should_quit);

        dash.mcp_screen = true;
        dash.handle_key(key(KeyCode::Char('q')));
        assert!(dash.should_quit, "q means quit here like everywhere else");
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
    fn d_removes_the_selected_server_after_confirming() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = mcp_dash(dir.path());
        dash.mcp_add_from_line("gone npx");
        dash.refresh_mcp_rows();
        dash.mcp_screen = true;
        dash.mcp_selected = 0;
        dash.handle_key(key(KeyCode::Char('d')));
        assert!(dash.pending_confirm.is_some(), "d asks first");
        assert_eq!(dash.mcp_rows.len(), 1, "nothing removed yet");
        dash.handle_key(key(KeyCode::Char('y')));
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

    // ─── the new-run screen ───────────────────────────────────────────────

    /// A dashboard whose new-run screen reads from an empty temp tree, so `n`
    /// never scans the real agents directory or working directory.
    fn new_run_dash(dir: &std::path::Path) -> Dashboard {
        let mut dash = make_test_dashboard();
        dash.new_run_ctx = NewRunContext {
            agents_dir: dir.join("agents"),
            config_path: dir.join("config.toml"),
            workdir: dir.join("work"),
        };
        dash
    }

    #[test]
    fn n_opens_the_new_run_screen() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = new_run_dash(dir.path());
        dash.handle_key(key(KeyCode::Char('n')));
        assert!(dash.new_run_screen);
    }

    #[test]
    fn the_new_run_screen_owns_the_keys_while_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut dash = new_run_dash(dir.path());
        dash.handle_key(key(KeyCode::Char('n')));
        // `q` would quit from the main list; here it is filter text, because a
        // task is typed on this screen.
        dash.handle_key(key(KeyCode::Char('q')));
        assert!(!dash.should_quit);
        assert_eq!(dash.new_run_filter, "q");

        dash.handle_key(key(KeyCode::Esc));
        dash.handle_key(key(KeyCode::Esc));
        assert!(!dash.new_run_screen);
    }

    // ─── where you end up after starting a run ───────────────────────────────

    /// The whole path: list → new run → the run's own page → back to the list.
    ///
    /// The last step is the one worth pinning. The new-run screen closes at submit
    /// rather than staying behind the detail view, so Esc from the run lands on
    /// the list and not back in a form that has already been filled in.
    #[test]
    fn starting_a_run_opens_its_page_and_esc_returns_to_the_list() {
        let mut dash = make_test_dashboard();
        dash.handle_key(key(KeyCode::Char('n')));
        assert!(dash.new_run_screen, "n opens the screen");

        // The run is dispatched; the daemon has not reported it yet.
        dash.close_new_run_screen();
        dash.inject_spawn_outcome_for_test(SpawnOutcome {
            message: "Started run-7".to_string(),
            ok: true,
            run_id: Some("run-7".to_string()),
        });
        dash.drain_spawn_outcomes();
        dash.open_pending_run();
        assert!(
            !dash.detail_view,
            "nothing to open until the run reaches the list"
        );

        // It arrives on a later sync.
        dash.agents
            .push(make_test_agent("run-7", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.open_pending_run();
        assert!(dash.detail_view, "the run's page opens by itself");
        assert_eq!(
            dash.selected_agent().map(|a| a.id.as_str()),
            Some("run-7"),
            "and it is the run that was started, not whatever was selected"
        );

        // Back goes to the list, not to the screen the run was started from.
        dash.handle_key(key(KeyCode::Esc));
        assert!(!dash.detail_view);
        assert!(!dash.new_run_screen, "the form is not on the back stack");
    }

    /// A run that never appears is given up on rather than opening a page minutes
    /// later, under whatever the user has moved on to.
    #[test]
    fn a_run_that_never_arrives_is_dropped() {
        let mut dash = make_test_dashboard();
        dash.inject_spawn_outcome_for_test(SpawnOutcome {
            message: "Started ghost".to_string(),
            ok: true,
            run_id: Some("ghost".to_string()),
        });
        dash.drain_spawn_outcomes();
        for _ in 0..crate::commands::dashboard::new_run::OPEN_RUN_TICKS + 1 {
            dash.open_pending_run();
        }
        assert!(dash.pending_open_run.is_none(), "it stopped waiting");
        assert!(!dash.detail_view);
    }

    /// Somewhere else means the user went there themselves.
    #[test]
    fn a_pending_run_does_not_yank_you_out_of_another_screen() {
        let mut dash = make_test_dashboard();
        dash.inject_spawn_outcome_for_test(SpawnOutcome {
            message: "Started run-7".to_string(),
            ok: true,
            run_id: Some("run-7".to_string()),
        });
        dash.drain_spawn_outcomes();
        dash.agents
            .push(make_test_agent("run-7", AgentDisplayStatus::Active));
        dash.update_display_indices();

        dash.mcp_screen = true;
        dash.open_pending_run();
        assert!(!dash.detail_view, "it waits rather than interrupting");
        assert!(dash.mcp_screen);
    }

    /// A spawn that failed has no page to open.
    #[test]
    fn a_refused_run_opens_nothing() {
        let mut dash = make_test_dashboard();
        dash.inject_spawn_outcome_for_test(SpawnOutcome {
            message: "The daemon refused the run: nope".to_string(),
            ok: false,
            run_id: None,
        });
        dash.drain_spawn_outcomes();
        assert!(dash.pending_open_run.is_none());
        dash.open_pending_run();
        assert!(!dash.detail_view);
    }

    // ── Marking runs with Space for group kill / delete ──────────────────

    #[test]
    fn space_marks_the_selected_run_and_advances() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.agents
            .push(make_test_agent("run-2", AgentDisplayStatus::Active));
        dash.update_display_indices();
        let first_id = dash.selected_agent().unwrap().id.clone();
        dash.handle_key(key(KeyCode::Char(' ')));
        assert!(dash.marked.contains(&first_id));
        assert_eq!(dash.selected, 1, "space moves down like the Down key");
    }

    #[test]
    fn space_on_a_marked_run_unmarks_it() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.agents
            .push(make_test_agent("run-2", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Char(' ')));
        dash.handle_key(key(KeyCode::Up));
        dash.handle_key(key(KeyCode::Char(' ')));
        assert!(dash.marked.is_empty(), "the second press toggles it off");
    }

    #[test]
    fn space_on_the_last_row_keeps_the_selection() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(dash.marked.len(), 1);
        assert_eq!(dash.selected, 0, "nothing below to advance to");
    }

    #[test]
    fn space_with_no_runs_is_a_noop() {
        let mut dash = make_test_dashboard();
        dash.handle_key(key(KeyCode::Char(' ')));
        assert!(dash.marked.is_empty());
    }

    #[test]
    fn esc_clears_marks_when_no_filter_is_set() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(dash.marked.len(), 1);
        dash.handle_key(key(KeyCode::Esc));
        assert!(dash.marked.is_empty());
        assert!(!dash.should_quit);
    }

    #[test]
    fn esc_clears_the_filter_before_the_marks() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.update_display_indices();
        dash.handle_key(key(KeyCode::Char(' ')));
        dash.list_search_query = "test".to_string();
        dash.handle_key(key(KeyCode::Esc));
        assert!(dash.list_search_query.is_empty());
        assert_eq!(dash.marked.len(), 1, "the first Esc only drops the filter");
        dash.handle_key(key(KeyCode::Esc));
        assert!(dash.marked.is_empty());
    }

    #[test]
    fn marked_delete_confirm_carries_all_ids_and_removes_them() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Complete));
        dash.agents
            .push(make_test_agent("run-2", AgentDisplayStatus::Complete));
        dash.agents
            .push(make_test_agent("run-3", AgentDisplayStatus::Complete));
        dash.update_display_indices();
        dash.marked.insert("run-1".to_string());
        dash.marked.insert("run-2".to_string());
        dash.handle_key(key(KeyCode::Char('d')));

        let (action, dialog) = dash.pending_confirm.clone().expect("d opens the dialog");
        assert_eq!(
            action,
            ConfirmAction::Delete {
                run_ids: vec!["run-1".to_string(), "run-2".to_string()],
            }
        );
        assert!(
            format!("{:?}", dialog.body).contains("Delete 2 runs"),
            "the body states the count: {:?}",
            dialog.body
        );

        dash.handle_key(key(KeyCode::Char('y')));
        assert_eq!(dash.agents.len(), 1, "both marked runs are gone");
        assert_eq!(dash.agents[0].id, "run-3");
        assert!(dash.marked.is_empty(), "acted-on ids leave the mark set");
    }

    #[test]
    fn marked_delete_of_one_run_uses_the_singular_noun() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Complete));
        dash.update_display_indices();
        dash.marked.insert("run-1".to_string());
        dash.handle_key(key(KeyCode::Char('d')));
        let (_, dialog) = dash.pending_confirm.clone().expect("d opens the dialog");
        assert!(
            format!("{:?}", dialog.body).contains("Delete 1 run and its"),
            "{:?}",
            dialog.body
        );
    }

    #[test]
    fn marked_kill_skips_finished_runs_and_unmarks_only_the_killed() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.agents
            .push(make_test_agent("run-2", AgentDisplayStatus::Complete));
        dash.update_display_indices();
        dash.marked.insert("run-1".to_string());
        dash.marked.insert("run-2".to_string());
        dash.handle_key(key(KeyCode::Char('x')));

        let (action, dialog) = dash.pending_confirm.clone().expect("x opens the dialog");
        assert_eq!(
            action,
            ConfirmAction::Kill {
                run_ids: vec!["run-1".to_string()],
            },
            "the finished run is not on the kill list"
        );
        assert!(
            format!("{:?}", dialog.body).contains("Cancel 1 run?"),
            "{:?}",
            dialog.body
        );

        dash.handle_key(key(KeyCode::Char('y')));
        assert_cancelled(&dash.agents[0].status);
        assert_eq!(
            cmd_rx.try_recv().unwrap(),
            DaemonCommand::Cancel {
                run_id: "run-1".to_string()
            }
        );
        assert!(cmd_rx.try_recv().is_err(), "run-2 was never cancelled");
        assert!(!dash.marked.contains("run-1"), "the killed run is unmarked");
        assert!(
            dash.marked.contains("run-2"),
            "the skipped run stays marked"
        );
    }

    #[test]
    fn marked_kill_of_several_runs_cancels_each() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let mut dash = Dashboard::new(cmd_tx);
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Active));
        dash.agents
            .push(make_test_agent("run-2", AgentDisplayStatus::Waiting));
        dash.update_display_indices();
        dash.marked.insert("run-1".to_string());
        dash.marked.insert("run-2".to_string());
        dash.handle_key(key(KeyCode::Char('x')));

        let (_, dialog) = dash.pending_confirm.clone().expect("x opens the dialog");
        assert!(
            format!("{:?}", dialog.body).contains("Cancel 2 runs?"),
            "{:?}",
            dialog.body
        );

        dash.handle_key(key(KeyCode::Char('y')));
        assert_cancelled(&dash.agents[0].status);
        assert_cancelled(&dash.agents[1].status);
        assert!(cmd_rx.try_recv().is_ok());
        assert!(cmd_rx.try_recv().is_ok());
        assert!(dash.marked.is_empty());
    }

    #[test]
    fn marked_kill_with_only_finished_runs_opens_no_dialog() {
        let mut dash = make_test_dashboard();
        dash.agents
            .push(make_test_agent("run-1", AgentDisplayStatus::Complete));
        dash.update_display_indices();
        dash.marked.insert("run-1".to_string());
        dash.handle_key(key(KeyCode::Char('x')));
        assert!(dash.pending_confirm.is_none());
    }
}
