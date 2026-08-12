//! Key handling for the setup wizard.
//!
//! One entry point, [`Wizard::handle_key`], with a strict priority order:
//! Ctrl-C, then an open confirmation dialog, then an open text edit, then the
//! help overlay, then navigation. Editing before navigation matters: while a
//! field is open, letters are letters, so `q` types a `q` rather than
//! quitting - losing a half-entered API key to a quit shortcut would be a
//! genuinely bad way to find out about modal input.
//!
//! Navigation resolves shared keys through `crate::tui::keymap`, so arrows,
//! vim aliases, Space, Enter, Esc, Tab, `?`, and `q` mean here exactly what
//! they mean in every other Leviath TUI. Enter acts on the focused row - it
//! toggles a provider, opens an editor, cycles a choice - and only advances
//! the screen when the cursor is visibly on the step's Continue button.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use super::catalog::Credential;
use super::state::{
    ConfirmPurpose, DetailAction, Edit, EditTarget, FieldValue, Picker, Step, Wizard,
};
use crate::tui::keymap;
use crate::tui::widgets::confirm::ConfirmOutcome;
use crate::tui::widgets::help::handle_help_key;
use crate::tui::widgets::line_edit::{EditOutcome, LineEdit};

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
        // Ctrl-C always works, even mid-edit and even inside a dialog: it is
        // the one binding a user reaches for expecting it to obey no matter
        // what. With unsaved choices it asks once; pressed again (the dialog
        // is then open), it quits unconditionally.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.confirm.is_some() || !self.dirty {
                self.should_quit = true;
            } else {
                self.open_quit_confirm();
            }
            return Action::Continue;
        }
        // Ctrl-R also works mid-edit: revealing what you are typing is most
        // useful precisely while you are typing it.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
            self.reveal = !self.reveal;
            self.message = Some(if self.reveal {
                "Credentials shown.".to_string()
            } else {
                "Credentials hidden.".to_string()
            });
            return Action::Continue;
        }
        if self.confirm.is_some() {
            return self.handle_confirm_key(key);
        }
        if let Some(picker) = self.picker.take() {
            self.handle_picker_key(key, picker);
            return Action::Continue;
        }
        if let Some(edit) = self.edit.take() {
            self.handle_edit_key(key, edit);
            return Action::Continue;
        }
        if self.show_help {
            if handle_help_key(&key, &self.help_scroll) {
                self.show_help = false;
            }
            return Action::Continue;
        }
        self.handle_nav_key(key)
    }

    /// Handle one mouse event against the window it was clicked in.
    ///
    /// A click acts on what it lands on rather than only selecting it, which
    /// is the point: the wizard leaned on `o` and `v` and a footer nobody
    /// read, and a row you can press is the version of that a first-time user
    /// finds on their own. Clicks are ignored while a dialog, an edit or the
    /// help overlay is up, because a click cannot mean anything there and
    /// dismissing them by accident would lose typed input.
    pub fn handle_mouse(&mut self, mouse: MouseEvent, area: Rect) -> Action {
        if self.confirm.is_some() || self.edit.is_some() || self.show_help {
            return Action::Continue;
        }
        if let Some(picker) = self.picker.take() {
            self.handle_picker_mouse(mouse, area, picker);
            return Action::Continue;
        }
        match mouse.kind {
            MouseEventKind::ScrollDown => self.scroll_by(1),
            MouseEventKind::ScrollUp => self.scroll_by(-1),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(row) = super::render::row_at(area, self, mouse.column, mouse.row) {
                    self.cursor = row;
                    return self.activate();
                }
            }
            _ => {}
        }
        Action::Continue
    }

    /// Keys while a confirmation dialog is open. Its Yes routes by purpose;
    /// No always just closes it.
    fn handle_confirm_key(&mut self, key: KeyEvent) -> Action {
        let Some(mut pending) = self.confirm.take() else {
            return Action::Continue;
        };
        match pending.dialog.handle(&key) {
            ConfirmOutcome::Pending => {
                self.confirm = Some(pending);
                Action::Continue
            }
            ConfirmOutcome::No => Action::Continue,
            ConfirmOutcome::Yes => match pending.purpose {
                ConfirmPurpose::QuitDiscard => {
                    self.should_quit = true;
                    Action::Continue
                }
                ConfirmPurpose::SaveTos => {
                    self.claude_code_tos_accepted = true;
                    Action::Save
                }
                ConfirmPurpose::NoProviders => {
                    self.next_step();
                    Action::Continue
                }
            },
        }
    }

    /// The mouse while the chooser is open: the wheel moves within the list, a
    /// click on a row takes it.
    ///
    /// A click outside the list is ignored rather than closing the chooser.
    /// Closing on a stray click would discard a search somebody was halfway
    /// through typing, and Esc is right there.
    fn handle_picker_mouse(&mut self, mouse: MouseEvent, area: Rect, mut picker: Picker) {
        match mouse.kind {
            MouseEventKind::ScrollDown => picker.move_cursor(1),
            MouseEventKind::ScrollUp => picker.move_cursor(-1),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(row) = super::render::picker_row_at(area, &picker, mouse.row) {
                    picker.cursor = row;
                    self.commit_picker(picker);
                    return;
                }
            }
            _ => {}
        }
        self.picker = Some(picker);
    }

    /// Keys while the chooser is open.
    ///
    /// Everything that is not navigation goes to the search box, so letters
    /// type rather than acting: `q` in a chooser means the user is looking for
    /// Qwen, and quitting setup instead would be indefensible.
    fn handle_picker_key(&mut self, key: KeyEvent, mut picker: Picker) {
        match key.code {
            KeyCode::Up => picker.move_cursor(-1),
            KeyCode::Down => picker.move_cursor(1),
            KeyCode::PageUp => picker.move_cursor(-Wizard::PAGE),
            KeyCode::PageDown => picker.move_cursor(Wizard::PAGE),
            KeyCode::Home => picker.cursor = 0,
            KeyCode::End => picker.move_cursor(isize::MAX),
            _ => {
                match picker.query.handle_key(&key) {
                    EditOutcome::Commit => {
                        self.commit_picker(picker);
                        return;
                    }
                    // Esc closes without choosing, leaving the field as it was.
                    EditOutcome::Cancel => return,
                    EditOutcome::Pending => {}
                }
                // The filter just changed under the cursor, so a selection
                // that has been filtered away must not linger off the end.
                picker.move_cursor(0);
            }
        }
        self.picker = Some(picker);
    }

    /// Keys while a text field is open.
    ///
    /// Takes the edit rather than re-reading `self.edit`: the caller already
    /// established there is one, so re-checking would add arms nothing can
    /// reach.
    fn handle_edit_key(&mut self, key: KeyEvent, mut edit: Edit) {
        match edit.line.handle_key(&key) {
            EditOutcome::Commit => {
                self.edit = Some(edit);
                self.commit_edit();
                self.message = None;
                self.dirty = true;
            }
            EditOutcome::Cancel => {
                self.message = Some("Edit cancelled.".to_string());
            }
            EditOutcome::Pending => self.edit = Some(edit),
        }
    }

    /// Keys while navigating: surface-specific bindings first, then the
    /// crate-wide keymap.
    fn handle_nav_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('s') if ctrl => return self.try_save(),
            KeyCode::Char('o') => self.open_signup_page(),
            KeyCode::Char('v') => self.verify_current(),
            KeyCode::PageUp => self.scroll_by(-Wizard::PAGE),
            KeyCode::PageDown => self.scroll_by(Wizard::PAGE),
            KeyCode::Home => self.scroll_home(),
            KeyCode::End => self.scroll_end(),
            // `?` reaches the keymap below; F1 has no keymap action and is
            // the key that works on the screens where `?` is text.
            KeyCode::F(1) => self.show_help = true,
            _ => match keymap::resolve(&key) {
                Some(keymap::Action::Up) => self.move_cursor(-1),
                Some(keymap::Action::Down) => self.move_cursor(1),
                Some(keymap::Action::Left) => self.adjust(-1),
                Some(keymap::Action::Right) => self.adjust(1),
                Some(keymap::Action::Toggle) => self.toggle(),
                Some(keymap::Action::Activate) => return self.activate(),
                Some(keymap::Action::Back) | Some(keymap::Action::Prev) => self.back(),
                Some(keymap::Action::Next) => self.forward_guarded(),
                Some(keymap::Action::Help) => self.show_help = true,
                Some(keymap::Action::Quit) => self.request_quit(),
                // Ctrl-C is intercepted in `handle_key`; this arm only fires
                // when `handle_nav_key` is driven directly (tests do).
                Some(keymap::Action::ForceQuit) => self.should_quit = true,
                None => {}
            },
        }
        Action::Continue
    }

    /// `q`: quit - after a confirmation when there are unsaved choices.
    fn request_quit(&mut self) {
        if self.dirty {
            self.open_quit_confirm();
        } else {
            self.should_quit = true;
        }
    }

    /// Save, unless the Claude Code terms still need confirming first.
    fn try_save(&mut self) -> Action {
        if self.needs_tos_confirmation() {
            self.open_tos_confirm();
            return Action::Continue;
        }
        Action::Save
    }

    /// `Enter`: act on the focused row, or - only from the visible Continue
    /// button - move on.
    fn activate(&mut self) -> Action {
        if self.on_continue() {
            return match self.step {
                Step::Review => self.try_save(),
                Step::Providers => {
                    self.forward_guarded();
                    Action::Continue
                }
                _ => {
                    self.forward();
                    Action::Continue
                }
            };
        }
        match self.step {
            Step::Providers | Step::Agents | Step::Mcp => self.toggle(),
            Step::ProviderDetail => match self.detail_actions().get(self.cursor.wrapping_sub(1)) {
                // Row 0 is the credential: it opens its editor, and the Claude
                // Code row has nothing to type, so Enter cycles its effort.
                // `wrapping_sub` turns that row into an index no action has.
                None => {
                    if !self.open_credential_editor() {
                        self.adjust(1);
                    }
                }
                Some(DetailAction::OpenSignup) => self.open_signup_page(),
                Some(DetailAction::Verify) => self.verify_current(),
            },
            Step::Defaults | Step::Limits => self.activate_field(),
            // Rowless steps put the cursor on their button, so these arms are
            // reachable only with a hand-forced cursor; acting on nothing is
            // correct then.
            Step::Welcome | Step::Review => {}
        }
        Action::Continue
    }

    /// Enter on a Defaults/Limits row always acts on that row's kind: toggle
    /// a bool, cycle a choice, open the editor for a number.
    fn activate_field(&mut self) {
        // The choice is cloned out before acting, because opening the chooser
        // needs `&mut self` while the field it came from is still borrowed.
        let Some(field) = self.fields().get(self.cursor) else {
            // Reachable only with a hand-forced cursor past the fields.
            return;
        };
        let label = field.label;
        let choice = match &field.value {
            FieldValue::Bool(_) => {
                self.toggle();
                return;
            }
            FieldValue::Number(_) => {
                self.open_field_editor();
                return;
            }
            FieldValue::Choice { options, index } => (options.clone(), *index),
        };
        // Unconditionally, because the only list-valued fields in the wizard
        // are the Defaults screen's provider and model. The tuning screen is
        // numbers and switches, which the arrows already handle well.
        self.open_picker(label, choice.0, choice.1);
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
            line: LineEdit::new(value, credential == Credential::ApiKey),
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
            line: LineEdit::new(buffer, false),
        });
        true
    }

    /// `Space` (or Enter on a row): toggle whatever the cursor is on.
    fn toggle(&mut self) {
        match self.step {
            Step::Providers => {
                if let Some(row) = self.providers.get_mut(self.cursor) {
                    row.selected = !row.selected;
                    self.dirty = true;
                    // Deselecting the Claude Code transport withdraws the
                    // terms acceptance so it must be re-confirmed if
                    // re-enabled.
                    if row.provider.id == "claude-code" && !row.selected {
                        self.claude_code_tos_accepted = false;
                    }
                }
                // The credential screen walks selected providers, so its
                // position is only meaningful relative to the current
                // selection.
                self.detail = 0;
            }
            Step::Agents => {
                if let Some(row) = self.agents.get_mut(self.cursor) {
                    row.selected = !row.selected;
                    self.dirty = true;
                }
            }
            Step::Mcp => {
                if let Some(row) = self.mcp.get_mut(self.cursor) {
                    row.selected = !row.selected;
                    self.dirty = true;
                }
            }
            Step::Defaults | Step::Limits => {
                let cursor = self.cursor;
                let mut changed = false;
                if let Some(fields) = self.fields_mut()
                    && let Some(field) = fields.get_mut(cursor)
                    && let FieldValue::Bool(b) = &mut field.value
                {
                    *b = !*b;
                    changed = true;
                }
                if changed {
                    self.dirty = true;
                    // One of those booleans decides whether the tuning screen
                    // is on the path at all. The field was built from the flag,
                    // so flipping one flips the other, and the Continue
                    // button's label changes with it.
                    if self.step == Step::Defaults && cursor == Wizard::ADVANCED_FIELD {
                        self.show_advanced = !self.show_advanced;
                    }
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
                    self.dirty = true;
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
                if changed_provider {
                    self.dirty = true;
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

    /// Advance, but guard the one advance that is almost always a slip:
    /// leaving the Providers screen with nothing selected.
    fn forward_guarded(&mut self) {
        if self.step == Step::Providers && self.selected_providers().is_empty() {
            self.open_no_providers_confirm();
            return;
        }
        self.forward();
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
    /// real browser - `lev dash` learned that the hard way when a unit test
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
mod tests;
