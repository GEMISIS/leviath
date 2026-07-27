//! Key handling for the setup wizard.
//!
//! One entry point, [`Wizard::handle_key`], split into a text-editing mode and
//! a navigation mode. Editing takes priority: while a field is open, letters
//! are letters, so `q` types a `q` rather than quitting — losing a
//! half-entered API key to a quit shortcut would be a genuinely bad way to find
//! out about modal input.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::catalog::Credential;
use super::state::{Edit, EditTarget, FieldValue, Step, Wizard};

/// What the loop should do after a key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Keep going.
    Continue,
    /// Apply the plan, then stop.
    Save,
}

impl Wizard {
    /// Handle one key press.
    pub fn handle_key(&mut self, key: KeyEvent) -> Action {
        // Ctrl-C always quits, even mid-edit: it is the one binding a user
        // reaches for expecting it to work no matter what.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return Action::Continue;
        }
        if let Some(edit) = self.edit.take() {
            self.handle_edit_key(key, edit);
            return Action::Continue;
        }
        if self.show_help {
            // Any key dismisses the help overlay.
            self.show_help = false;
            return Action::Continue;
        }
        self.handle_nav_key(key)
    }

    /// Keys while a text field is open.
    ///
    /// Takes the buffer rather than re-reading `self.edit`: the caller already
    /// established there is one, so re-checking would add arms nothing can
    /// reach.
    fn handle_edit_key(&mut self, key: KeyEvent, mut edit: Edit) {
        match key.code {
            KeyCode::Enter => {
                self.edit = Some(edit);
                self.commit_edit();
                self.message = None;
            }
            KeyCode::Esc => {
                self.message = Some("Edit cancelled.".to_string());
            }
            KeyCode::Backspace => {
                edit.buffer.pop();
                self.edit = Some(edit);
            }
            KeyCode::Char(c) => {
                edit.buffer.push(c);
                self.edit = Some(edit);
            }
            _ => self.edit = Some(edit),
        }
    }

    /// Keys while navigating.
    fn handle_nav_key(&mut self, key: KeyEvent) -> Action {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        match key.code {
            KeyCode::Char('s') if ctrl => return Action::Save,
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('r') if ctrl => {
                self.reveal = !self.reveal;
                self.message = Some(if self.reveal {
                    "Credentials shown.".to_string()
                } else {
                    "Credentials hidden.".to_string()
                });
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::Left | KeyCode::Char('h') => self.adjust(-1),
            KeyCode::Right | KeyCode::Char('l') => self.adjust(1),
            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Char('o') => self.open_signup_page(),
            KeyCode::Char('v') => self.verify_current(),
            KeyCode::BackTab => self.back(),
            KeyCode::Tab if shift => self.back(),
            KeyCode::Tab => self.forward(),
            KeyCode::Esc => self.back(),
            KeyCode::Enter => return self.activate(),
            _ => {}
        }
        Action::Continue
    }

    /// `Enter`: open an editor, or advance.
    ///
    /// A screen with nothing to type into — a toggle, a choice, a list — falls
    /// through to "next screen" rather than doing nothing at all, so Enter is
    /// always the key that moves forward.
    fn activate(&mut self) -> Action {
        let opened = match self.step {
            Step::Review => return Action::Save,
            Step::ProviderDetail => self.open_credential_editor(),
            Step::Defaults | Step::Limits => self.open_field_editor(),
            _ => false,
        };
        if opened {
            return Action::Continue;
        }
        self.forward();
        Action::Continue
    }

    /// Open the credential editor for the provider on screen. Returns false
    /// when this provider has nothing to type (Claude Code).
    fn open_credential_editor(&mut self) -> bool {
        let Some((index, credential, value)) = self.detail_row().map(|index| {
            let row = &self.providers[index];
            (index, row.provider.credential, row.value.clone())
        }) else {
            return false;
        };
        if credential == Credential::None {
            return false;
        }
        self.edit = Some(Edit {
            target: EditTarget::Credential(index),
            buffer: value,
            masked: credential == Credential::ApiKey,
        });
        true
    }

    /// Open the text editor for the selected field. Returns false for fields
    /// that are not text.
    fn open_field_editor(&mut self) -> bool {
        let cursor = self.cursor;
        let Some(FieldValue::Number(current)) = self.fields().get(cursor).map(|f| &f.value) else {
            return false;
        };
        let buffer = current.map(|n| n.to_string()).unwrap_or_default();
        self.edit = Some(Edit {
            target: EditTarget::Field(cursor),
            buffer,
            masked: false,
        });
        true
    }

    /// `Space`: toggle whatever the cursor is on.
    fn toggle(&mut self) {
        match self.step {
            Step::Providers => {
                if let Some(row) = self.providers.get_mut(self.cursor) {
                    row.selected = !row.selected;
                }
                // The credential screen walks selected providers, so its
                // position is only meaningful relative to the current
                // selection.
                self.detail = 0;
            }
            Step::Agents => {
                if let Some(row) = self.agents.get_mut(self.cursor) {
                    row.selected = !row.selected;
                }
            }
            Step::Mcp => {
                if let Some(row) = self.mcp.get_mut(self.cursor) {
                    row.selected = !row.selected;
                }
            }
            Step::Defaults | Step::Limits => {
                let cursor = self.cursor;
                if let Some(fields) = self.fields_mut()
                    && let Some(field) = fields.get_mut(cursor)
                    && let FieldValue::Bool(b) = &mut field.value
                {
                    *b = !*b;
                }
            }
            Step::Welcome | Step::ProviderDetail | Step::Review => {}
        }
    }

    /// `←`/`→`: cycle a choice, or step through the credential screen's
    /// providers.
    fn adjust(&mut self, delta: isize) {
        match self.step {
            Step::ProviderDetail => {
                // The effort selector is the only cyclable value here.
                if let Some(index) = self.detail_row()
                    && let Some(row) = self.providers.get_mut(index)
                    && row.provider.credential == Credential::None
                {
                    let count = super::state::effort_options().len();
                    let next = row.effort as isize + delta;
                    row.effort = next.rem_euclid(count as isize) as usize;
                }
            }
            Step::Defaults | Step::Limits => {
                let cursor = self.cursor;
                let mut changed_provider = false;
                if let Some(fields) = self.fields_mut()
                    && let Some(field) = fields.get_mut(cursor)
                    && let FieldValue::Choice { options, index } = &mut field.value
                    && !options.is_empty()
                {
                    let next = *index as isize + delta;
                    *index = next.rem_euclid(options.len() as isize) as usize;
                    changed_provider = true;
                }
                // Changing the default provider re-picks the concurrency
                // default, so an Ollama-first setup does not inherit a number
                // meant for hosted APIs.
                if changed_provider && self.step == Step::Defaults && cursor == 0 {
                    self.apply_provider_concurrency_default();
                }
            }
            _ => {}
        }
    }

    /// `Tab`: next provider on the credential screen, otherwise next step.
    fn forward(&mut self) {
        if self.step == Step::ProviderDetail {
            // Verify what was just entered before moving on, so the answer is
            // waiting rather than starting when the user asks for it.
            if let Some(index) = self.detail_row() {
                self.request_verification(index);
            }
            if self.next_detail() {
                return;
            }
        }
        self.next_step();
    }

    /// `Esc` / `Shift-Tab`: previous provider, otherwise previous step.
    fn back(&mut self) {
        if self.step == Step::ProviderDetail && self.prev_detail() {
            return;
        }
        self.prev_step();
    }

    /// `v`: re-check the provider on screen, or every selected one.
    fn verify_current(&mut self) {
        match self.step {
            Step::ProviderDetail => {
                if let Some(index) = self.detail_row() {
                    self.request_verification(index);
                    self.message = Some("Checking…".to_string());
                }
            }
            Step::Providers | Step::Review => {
                self.verify_all();
                self.message = Some("Checking every selected provider…".to_string());
            }
            _ => {}
        }
    }

    /// `o`: open the current provider's signup page.
    ///
    /// The opener is a field rather than a direct call so tests never launch a
    /// real browser — `lev dash` learned that the hard way when a unit test
    /// opened one.
    fn open_signup_page(&mut self) {
        let url = match self.step {
            Step::ProviderDetail => self
                .detail_row()
                .and_then(|i| self.providers.get(i))
                .and_then(|r| r.provider.signup_url),
            Step::Providers => self
                .providers
                .get(self.cursor)
                .and_then(|r| r.provider.signup_url),
            _ => None,
        };
        match url {
            Some(url) => {
                let opened = (self.opener)(url);
                self.message = Some(if opened {
                    format!("Opened {url}")
                } else {
                    format!("Couldn't open a browser. Visit {url}")
                });
            }
            None => self.message = Some("Nothing to open here.".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::setup::state::{Edit, EditTarget, FieldValue};
    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn press_with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn wizard() -> (tempfile::TempDir, Wizard) {
        let dir = tempfile::tempdir().unwrap();
        let wizard = crate::commands::setup::state::tests::test_wizard(dir.path());
        (dir, wizard)
    }

    // ─── quitting and saving ────────────────────────────────────────────────

    #[test]
    fn ctrl_c_quits_from_anywhere_including_mid_edit() {
        // The one binding a user reaches for expecting it to always work.
        let (_dir, mut w) = wizard();
        w.edit = Some(Edit {
            target: EditTarget::Credential(0),
            buffer: "half-typed".to_string(),
            masked: true,
        });

        let action = w.handle_key(press_with(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert_eq!(action, Action::Continue);
        assert!(w.should_quit);
    }

    #[test]
    fn q_quits_while_navigating() {
        let (_dir, mut w) = wizard();
        w.handle_key(press(KeyCode::Char('q')));
        assert!(w.should_quit);
    }

    #[test]
    fn q_types_a_letter_while_editing_rather_than_quitting() {
        // Losing a half-entered API key to a quit shortcut would be a bad way
        // to find out about modal input.
        let (_dir, mut w) = wizard();
        w.edit = Some(Edit {
            target: EditTarget::Credential(0),
            buffer: String::new(),
            masked: true,
        });

        w.handle_key(press(KeyCode::Char('q')));

        assert!(!w.should_quit);
        assert_eq!(w.edit.as_ref().expect("still editing").buffer, "q");
    }

    #[test]
    fn ctrl_s_saves_from_any_screen() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Providers);

        let action = w.handle_key(press_with(KeyCode::Char('s'), KeyModifiers::CONTROL));

        assert_eq!(action, Action::Save);
    }

    #[test]
    fn enter_on_the_review_screen_saves() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Review);

        assert_eq!(w.handle_key(press(KeyCode::Enter)), Action::Save);
    }

    // ─── help overlay ───────────────────────────────────────────────────────

    #[test]
    fn the_help_overlay_opens_and_any_key_closes_it() {
        let (_dir, mut w) = wizard();

        w.handle_key(press(KeyCode::Char('?')));
        assert!(w.show_help);

        // The dismissing key must not also do its normal job.
        w.handle_key(press(KeyCode::Char('q')));
        assert!(!w.show_help);
        assert!(!w.should_quit);
    }

    // ─── editing ────────────────────────────────────────────────────────────

    #[test]
    fn typing_backspacing_and_committing_a_credential() {
        let (_dir, mut w) = wizard();
        w.providers[0].selected = true;
        w.enter(Step::ProviderDetail);

        w.handle_key(press(KeyCode::Enter));
        assert!(w.edit.is_some(), "Enter opens the editor");
        for c in "sk-antX".chars() {
            w.handle_key(press(KeyCode::Char(c)));
        }
        w.handle_key(press(KeyCode::Backspace));
        w.handle_key(press(KeyCode::Enter));

        assert!(w.edit.is_none());
        assert_eq!(w.providers[0].value, "sk-ant");
    }

    #[test]
    fn escape_abandons_an_edit_without_changing_the_value() {
        let (_dir, mut w) = wizard();
        w.providers[0].selected = true;
        w.providers[0].value = "sk-ant-original".to_string();
        w.enter(Step::ProviderDetail);
        w.handle_key(press(KeyCode::Enter));
        w.handle_key(press(KeyCode::Char('z')));

        w.handle_key(press(KeyCode::Esc));

        assert!(w.edit.is_none());
        assert_eq!(w.providers[0].value, "sk-ant-original");
        assert_eq!(w.message.as_deref(), Some("Edit cancelled."));
    }

    #[test]
    fn an_unhandled_key_while_editing_is_ignored() {
        let (_dir, mut w) = wizard();
        w.edit = Some(Edit {
            target: EditTarget::Credential(0),
            buffer: "abc".to_string(),
            masked: false,
        });

        w.handle_key(press(KeyCode::F(5)));

        assert_eq!(w.edit.as_ref().expect("still editing").buffer, "abc");
    }

    #[test]
    fn backspace_on_an_empty_buffer_is_harmless() {
        let (_dir, mut w) = wizard();
        w.edit = Some(Edit {
            target: EditTarget::Credential(0),
            buffer: String::new(),
            masked: false,
        });

        w.handle_key(press(KeyCode::Backspace));

        assert!(w.edit.as_ref().expect("still editing").buffer.is_empty());
    }

    #[test]
    fn the_claude_code_transport_has_nothing_to_type_so_enter_moves_on() {
        let (_dir, mut w) = wizard();
        let index = w
            .providers
            .iter()
            .position(|r| r.provider.id == "claude-code")
            .expect("the transport is offered");
        w.providers[index].selected = true;
        w.enter(Step::ProviderDetail);

        w.handle_key(press(KeyCode::Enter));

        assert!(w.edit.is_none());
        assert_ne!(w.step, Step::ProviderDetail);
    }

    #[test]
    fn enter_on_a_toggle_moves_on_rather_than_doing_nothing() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Limits);
        w.cursor = 3; // a boolean

        w.handle_key(press(KeyCode::Enter));

        assert!(w.edit.is_none());
        assert_ne!(w.step, Step::Limits);
    }

    #[test]
    fn enter_on_a_number_field_opens_it_seeded_with_the_current_value() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Limits);

        w.handle_key(press(KeyCode::Enter));

        let edit = w.edit.as_ref().expect("the editor opened");
        assert_eq!(edit.target, EditTarget::Field(0));
        assert!(!edit.buffer.is_empty(), "seeded from the current value");
        assert!(!edit.masked, "a limit is not a secret");
    }

    #[test]
    fn an_unset_number_field_opens_empty() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Limits);
        w.limits[0].value = FieldValue::Number(None);

        w.handle_key(press(KeyCode::Enter));

        assert!(
            w.edit
                .as_ref()
                .expect("the editor opened")
                .buffer
                .is_empty()
        );
    }

    #[test]
    fn opening_an_editor_out_of_range_is_a_no_op() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Limits);
        w.cursor = 99;

        w.handle_key(press(KeyCode::Enter));

        assert!(w.edit.is_none());
    }

    #[test]
    fn the_credential_editor_declines_when_no_provider_is_on_screen() {
        let (_dir, mut w) = wizard();
        w.enter(Step::ProviderDetail);

        w.handle_key(press(KeyCode::Enter));

        assert!(w.edit.is_none());
    }

    // ─── selection ──────────────────────────────────────────────────────────

    #[test]
    fn space_toggles_a_provider_and_resets_the_credential_walk() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Providers);
        w.detail = 3;

        w.handle_key(press(KeyCode::Char(' ')));

        assert!(w.providers[0].selected);
        assert_eq!(w.detail, 0, "the walk is relative to the new selection");

        w.handle_key(press(KeyCode::Char(' ')));
        assert!(!w.providers[0].selected);
    }

    #[test]
    fn space_toggles_agents_and_mcp_rows_and_booleans() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Wizard::new(
            crate::config::Config::default(),
            &|_| None,
            vec![(
                "A".to_string(),
                crate::commands::setup::import::Candidate {
                    config: leviath_mcp::MCPServerConfig::stdio("fs", "npx", vec![]),
                    scope: String::new(),
                    inline_secrets: Vec::new(),
                },
            )],
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| true),
        );

        w.enter(Step::Agents);
        let before = w.agents[0].selected;
        w.handle_key(press(KeyCode::Char(' ')));
        assert_ne!(w.agents[0].selected, before);

        w.enter(Step::Mcp);
        w.handle_key(press(KeyCode::Char(' ')));
        assert!(!w.mcp[0].selected);

        w.enter(Step::Limits);
        w.cursor = 3;
        let before = w.limits[3].value.clone();
        w.handle_key(press(KeyCode::Char(' ')));
        assert_ne!(w.limits[3].value, before);
    }

    #[test]
    fn space_on_a_screen_with_nothing_to_toggle_is_harmless() {
        let (_dir, mut w) = wizard();
        for step in [Step::Welcome, Step::ProviderDetail, Step::Review] {
            w.enter(step);
            w.handle_key(press(KeyCode::Char(' ')));
        }
        // Also out-of-range cursors on each list step.
        for step in [Step::Providers, Step::Agents, Step::Mcp] {
            w.enter(step);
            w.cursor = 999;
            w.handle_key(press(KeyCode::Char(' ')));
        }
        assert!(!w.should_quit);
    }

    // ─── movement ───────────────────────────────────────────────────────────

    #[test]
    fn both_arrow_and_vim_keys_move_the_cursor() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Providers);

        w.handle_key(press(KeyCode::Down));
        assert_eq!(w.cursor, 1);
        w.handle_key(press(KeyCode::Char('j')));
        assert_eq!(w.cursor, 2);
        w.handle_key(press(KeyCode::Up));
        assert_eq!(w.cursor, 1);
        w.handle_key(press(KeyCode::Char('k')));
        assert_eq!(w.cursor, 0);
    }

    #[test]
    fn left_and_right_cycle_a_choice_in_both_directions() {
        let (_dir, mut w) = wizard();
        w.providers[0].selected = true;
        let ollama = w
            .providers
            .iter()
            .position(|r| r.provider.id == "ollama")
            .expect("ollama is offered");
        w.providers[ollama].selected = true;
        w.enter(Step::Defaults);

        w.handle_key(press(KeyCode::Right));
        assert_eq!(w.defaults[0].value.display(), "ollama");
        w.handle_key(press(KeyCode::Char('h')));
        assert_eq!(w.defaults[0].value.display(), "anthropic");
        // Wrapping backwards from the first option lands on the last.
        w.handle_key(press(KeyCode::Left));
        assert_eq!(w.defaults[0].value.display(), "ollama");
    }

    #[test]
    fn choosing_ollama_as_the_default_lowers_the_concurrency_limit() {
        let (_dir, mut w) = wizard();
        w.providers[0].selected = true;
        let ollama = w
            .providers
            .iter()
            .position(|r| r.provider.id == "ollama")
            .expect("ollama is offered");
        w.providers[ollama].selected = true;
        w.enter(Step::Defaults);
        w.cursor = 0;

        w.handle_key(press(KeyCode::Right));

        assert_eq!(w.defaults[0].value.display(), "ollama");
        assert_eq!(
            w.limits[0].value,
            FieldValue::Number(Some(
                crate::commands::setup::catalog::OLLAMA_MAX_CONCURRENT_INFERENCES as u64
            ))
        );
    }

    #[test]
    fn left_and_right_cycle_the_claude_code_effort() {
        let (_dir, mut w) = wizard();
        let index = w
            .providers
            .iter()
            .position(|r| r.provider.id == "claude-code")
            .expect("the transport is offered");
        w.providers[index].selected = true;
        w.enter(Step::ProviderDetail);
        let before = w.providers[index].effort;

        w.handle_key(press(KeyCode::Right));
        assert_ne!(w.providers[index].effort, before);
        w.handle_key(press(KeyCode::Left));
        assert_eq!(w.providers[index].effort, before);
        // Wrapping backwards from the first level lands on the last.
        w.providers[index].effort = 0;
        w.handle_key(press(KeyCode::Left));
        assert_eq!(
            w.providers[index].effort,
            crate::commands::setup::state::effort_options().len() - 1
        );
    }

    #[test]
    fn arrows_on_a_keyed_provider_do_not_change_anything() {
        let (_dir, mut w) = wizard();
        w.providers[0].selected = true;
        w.enter(Step::ProviderDetail);
        let before = w.providers[0].effort;

        w.handle_key(press(KeyCode::Right));

        assert_eq!(w.providers[0].effort, before);
    }

    #[test]
    fn arrows_on_a_non_choice_field_or_empty_screen_are_harmless() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Limits);
        w.cursor = 0; // a number, not a choice
        w.handle_key(press(KeyCode::Right));
        assert_eq!(w.limits[0].value.display(), "8");

        w.enter(Step::Welcome);
        w.handle_key(press(KeyCode::Right));

        w.enter(Step::ProviderDetail);
        w.handle_key(press(KeyCode::Right));

        assert!(!w.should_quit);
    }

    #[test]
    fn an_empty_choice_list_does_not_divide_by_zero() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Defaults);
        w.defaults[0].value = FieldValue::Choice {
            options: Vec::new(),
            index: 0,
        };

        w.handle_key(press(KeyCode::Right));

        assert_eq!(w.defaults[0].value.display(), "(none)");
    }

    // ─── moving between screens ─────────────────────────────────────────────

    #[test]
    fn tab_and_shift_tab_walk_the_steps() {
        let (_dir, mut w) = wizard();

        w.handle_key(press(KeyCode::Tab));
        assert_eq!(w.step, Step::Providers);
        w.handle_key(press(KeyCode::BackTab));
        assert_eq!(w.step, Step::Welcome);
        w.handle_key(press(KeyCode::Tab));
        w.handle_key(press_with(KeyCode::Tab, KeyModifiers::SHIFT));
        assert_eq!(w.step, Step::Welcome);
    }

    #[test]
    fn escape_goes_back_a_step() {
        let (_dir, mut w) = wizard();
        w.enter(Step::Agents);

        w.handle_key(press(KeyCode::Esc));

        assert_eq!(w.step, Step::Limits);
    }

    #[test]
    fn tab_on_the_credential_screen_walks_providers_then_leaves() {
        let (_dir, mut w) = wizard();
        let (mut requests, _replies) = w.take_verify_ends().expect("first take");
        w.providers[0].selected = true;
        w.providers[0].value = "sk-ant".to_string();
        w.providers[1].selected = true;
        w.providers[1].value = "sk-oai".to_string();
        w.enter(Step::ProviderDetail);

        w.handle_key(press(KeyCode::Tab));
        assert_eq!(w.step, Step::ProviderDetail);
        assert_eq!(w.detail, 1, "moved to the second provider");

        w.handle_key(press(KeyCode::Tab));
        assert_eq!(w.step, Step::Defaults, "past the last provider");

        // Moving on verifies what was just entered, so the answer is waiting
        // rather than starting when the user asks for it.
        let mut checked = Vec::new();
        while let Ok(request) = requests.try_recv() {
            checked.push(request.provider_id);
        }
        assert_eq!(checked, vec!["anthropic", "openai"]);
    }

    #[test]
    fn escape_on_the_credential_screen_walks_back_through_providers() {
        let (_dir, mut w) = wizard();
        w.providers[0].selected = true;
        w.providers[1].selected = true;
        w.enter(Step::ProviderDetail);
        w.detail = 1;

        w.handle_key(press(KeyCode::Esc));
        assert_eq!(w.step, Step::ProviderDetail);
        assert_eq!(w.detail, 0);

        w.handle_key(press(KeyCode::Esc));
        assert_eq!(w.step, Step::Providers);
    }

    // ─── reveal, verify, open ───────────────────────────────────────────────

    #[test]
    fn ctrl_r_toggles_credential_visibility_and_says_which_way() {
        let (_dir, mut w) = wizard();

        w.handle_key(press_with(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert!(w.reveal);
        assert_eq!(w.message.as_deref(), Some("Credentials shown."));

        w.handle_key(press_with(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert!(!w.reveal);
        assert_eq!(w.message.as_deref(), Some("Credentials hidden."));
    }

    #[test]
    fn v_rechecks_the_provider_on_screen() {
        let (_dir, mut w) = wizard();
        let (mut requests, _replies) = w.take_verify_ends().expect("first take");
        w.providers[0].selected = true;
        w.providers[0].value = "sk-ant".to_string();
        w.enter(Step::ProviderDetail);

        w.handle_key(press(KeyCode::Char('v')));

        assert_eq!(w.message.as_deref(), Some("Checking…"));
        assert_eq!(
            requests.try_recv().expect("queued").provider_id,
            "anthropic"
        );
    }

    #[test]
    fn v_on_the_list_and_review_screens_rechecks_everything() {
        let (_dir, mut w) = wizard();
        let (mut requests, _replies) = w.take_verify_ends().expect("first take");
        w.providers[0].selected = true;
        w.providers[0].value = "sk-ant".to_string();

        w.enter(Step::Providers);
        w.handle_key(press(KeyCode::Char('v')));
        assert!(requests.try_recv().is_ok());

        w.enter(Step::Review);
        w.handle_key(press(KeyCode::Char('v')));
        assert!(requests.try_recv().is_ok());
    }

    #[test]
    fn v_elsewhere_does_nothing() {
        let (_dir, mut w) = wizard();
        let (mut requests, _replies) = w.take_verify_ends().expect("first take");
        w.enter(Step::Limits);

        w.handle_key(press(KeyCode::Char('v')));

        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn v_on_the_credential_screen_with_no_provider_does_nothing() {
        let (_dir, mut w) = wizard();
        w.enter(Step::ProviderDetail);

        w.handle_key(press(KeyCode::Char('v')));

        assert!(w.message.is_none());
    }

    #[test]
    fn o_opens_the_signup_page_from_both_provider_screens() {
        let dir = tempfile::tempdir().unwrap();
        let opened = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = opened.clone();
        let mut w = Wizard::new(
            crate::config::Config::default(),
            &|_| None,
            Vec::new(),
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(move |url: &str| {
                sink.lock().expect("not poisoned").push(url.to_string());
                true
            }),
        );

        w.enter(Step::Providers);
        w.handle_key(press(KeyCode::Char('o')));

        w.providers[0].selected = true;
        w.enter(Step::ProviderDetail);
        w.handle_key(press(KeyCode::Char('o')));

        let urls = opened.lock().expect("not poisoned").clone();
        assert_eq!(urls.len(), 2);
        assert!(urls.iter().all(|u| u.starts_with("https://")));
        assert!(
            w.message
                .as_deref()
                .unwrap_or_default()
                .starts_with("Opened"),
            "the user is told which page opened"
        );
    }

    #[test]
    fn a_browser_that_will_not_open_prints_the_url_instead() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Wizard::new(
            crate::config::Config::default(),
            &|_| None,
            Vec::new(),
            Vec::new(),
            dir.path(),
            std::sync::Arc::new(|_| false),
        );
        w.enter(Step::Providers);

        w.handle_key(press(KeyCode::Char('o')));

        let message = w.message.as_deref().unwrap_or_default();
        assert!(message.contains("Couldn't open"), "{message}");
        assert!(message.contains("https://"), "{message}");
    }

    #[test]
    fn o_where_there_is_nothing_to_open_says_so() {
        let (_dir, mut w) = wizard();
        let index = w
            .providers
            .iter()
            .position(|r| r.provider.id == "claude-code")
            .expect("the transport is offered");
        w.providers[index].selected = true;
        w.enter(Step::ProviderDetail);

        w.handle_key(press(KeyCode::Char('o')));
        assert_eq!(w.message.as_deref(), Some("Nothing to open here."));

        w.enter(Step::Limits);
        w.message = None;
        w.handle_key(press(KeyCode::Char('o')));
        assert_eq!(w.message.as_deref(), Some("Nothing to open here."));
    }

    #[test]
    fn enter_on_a_list_screen_moves_on_rather_than_opening_an_editor() {
        // Welcome, Providers, Agents and MCP have nothing to type into.
        for step in [Step::Welcome, Step::Providers, Step::Agents] {
            let (_dir, mut w) = wizard();
            w.enter(step);

            w.handle_key(press(KeyCode::Enter));

            assert!(w.edit.is_none());
            assert_ne!(w.step, step, "{step:?} should have advanced");
        }
    }

    #[test]
    fn space_on_a_number_field_leaves_it_alone() {
        // Only booleans toggle; a count would have nothing to toggle *to*.
        let (_dir, mut w) = wizard();
        w.enter(Step::Limits);
        w.cursor = 0;
        let before = w.limits[0].value.clone();

        w.handle_key(press(KeyCode::Char(' ')));

        assert_eq!(w.limits[0].value, before);
    }

    #[test]
    fn an_unbound_key_does_nothing() {
        let (_dir, mut w) = wizard();
        let before = w.step;

        w.handle_key(press(KeyCode::F(9)));

        assert_eq!(w.step, before);
        assert!(!w.should_quit);
    }
}
