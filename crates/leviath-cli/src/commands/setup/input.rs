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
    ConfirmPurpose, DetailAction, Edit, EditTarget, EndpointCursor, EndpointField, FieldValue,
    ModalButton, Picker, SigninAction, Step, Wizard,
};
use crate::tui::keymap;
use crate::tui::widgets::confirm::ConfirmOutcome;
use crate::tui::widgets::help::handle_help_key;
use crate::tui::widgets::line_edit::{EditOutcome, LineEdit};
use crate::tui::widgets::picker::PickerOutcome;
use crate::tui::widgets::reorder::{Reorder, ReorderOutcome};

/// What the loop should do after a key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// Keep going.
    Continue,
    /// Apply the plan, then stop.
    Save,
}

impl Wizard {
    /// Handle one key press.
    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Action {
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
        if let Some(reorder) = self.reorder.take() {
            self.handle_reorder_key(key, reorder);
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
        if self.modal.is_some() {
            self.handle_modal_key(key);
            return Action::Continue;
        }
        self.handle_nav_key(key)
    }

    /// Keys while a provider's setup modal is open: the arrows move over its
    /// card and buttons, Enter acts on what the cursor is on, Esc cancels,
    /// and `v` is the "Verify and use" button.
    fn handle_modal_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.cancel_modal(),
            KeyCode::Char('v') => self.activate_modal_button(ModalButton::VerifyUse),
            KeyCode::Char('o') => self.open_signup_page(),
            KeyCode::Char('s') if ctrl => {
                self.message = Some("Finish the provider first: use it, or cancel.".to_string());
            }
            KeyCode::F(1) => self.show_help = true,
            // The same jumps a screen has, over the card and its buttons.
            KeyCode::Home => self.scroll_home(),
            KeyCode::End => self.scroll_end(),
            KeyCode::PageUp => self.scroll_by(-Wizard::PAGE),
            KeyCode::PageDown => self.scroll_by(Wizard::PAGE),
            _ => match keymap::resolve(&key) {
                Some(keymap::Action::Up) => self.move_cursor(-1),
                Some(keymap::Action::Down) | Some(keymap::Action::Next) => self.move_cursor(1),
                Some(keymap::Action::Prev) => self.move_cursor(-1),
                Some(keymap::Action::Left) => self.adjust(-1),
                Some(keymap::Action::Right) => self.adjust(1),
                Some(keymap::Action::Activate) | Some(keymap::Action::Toggle) => {
                    self.activate_modal_row();
                }
                Some(keymap::Action::Help) => self.show_help = true,
                Some(keymap::Action::Quit) => self.request_quit(),
                // Esc is matched above as the cancel, and Ctrl-C is
                // intercepted in `handle_key`, so neither `Back` nor
                // `ForceQuit` reaches here.
                Some(keymap::Action::Back) | Some(keymap::Action::ForceQuit) | None => {}
            },
        }
    }

    /// Enter in the modal: a button does what it says, and the card's rows
    /// edit the credential, take a sign-in, or open a page. Only called
    /// while a modal is open.
    fn activate_modal_row(&mut self) {
        let index = self
            .modal_index()
            .expect("the modal's keys are handled only while it is open");
        if let Some(button) = self.modal_button_at(self.cursor) {
            self.activate_modal_button(button);
        } else if self.is_endpoint_preset(index) {
            self.activate_endpoint_row(index);
        } else {
            self.activate_detail_row(index);
        }
    }

    /// Finish, unless there is no provider to run anything with: a config
    /// with none cannot run an agent, so the wizard sends the user back to
    /// add one rather than write it.
    fn try_save(&mut self) -> Action {
        if self.selected_providers().is_empty() {
            self.message = Some("Add at least one provider before finishing.".to_string());
            self.enter(Step::Providers);
            return Action::Continue;
        }
        Action::Save
    }

    /// Handle one mouse event against the window it was clicked in.
    ///
    /// A click acts on what it lands on rather than only selecting it, which
    /// is the point: the wizard leaned on `o` and `v` and a footer nobody
    /// read, and a row you can press is the version of that a first-time user
    /// finds on their own. Clicks are ignored while a dialog, an edit or the
    /// help overlay is up, because a click cannot mean anything there and
    /// dismissing them by accident would lose typed input.
    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent, area: Rect) -> Action {
        // The setup modal is keyboard-driven: a click through it would land
        // on the Providers screen underneath.
        if self.confirm.is_some() || self.edit.is_some() || self.show_help || self.modal.is_some() {
            return Action::Continue;
        }
        if let Some(picker) = self.picker.take() {
            self.handle_picker_mouse(mouse, area, picker);
            return Action::Continue;
        }
        if let Some(reorder) = self.reorder.take() {
            self.handle_reorder_mouse(mouse, area, reorder);
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
            },
        }
    }

    /// The mouse while the chooser is open: the widget moves or chooses, and
    /// a choice is written back to the field.
    fn handle_picker_mouse(&mut self, mouse: MouseEvent, area: Rect, mut picker: Picker) {
        let outcome = picker.handle_mouse(&mouse, area);
        self.settle_picker(picker, outcome);
    }

    /// Keys while the chooser is open: the widget filters, moves or chooses.
    fn handle_picker_key(&mut self, key: KeyEvent, mut picker: Picker) {
        let outcome = picker.handle_key(&key);
        self.settle_picker(picker, outcome);
    }

    /// What the chooser decided: keep it open, or close it with or without a
    /// value. A choice is routed by what the chooser was for (a field, or a
    /// level of the add-a-provider flow); a dismissal steps that flow back a
    /// level. The wizard's chooser is single-select, so a many-choice never
    /// arrives; treated as a cancel should the widget ever send one.
    fn settle_picker(&mut self, picker: Picker, outcome: PickerOutcome) {
        match outcome {
            PickerOutcome::Pending => self.picker = Some(picker),
            PickerOutcome::Chosen(index) => self.settle_picker_choice(index),
            PickerOutcome::Cancelled | PickerOutcome::ChosenMany(_) => self.picker_back(),
        }
    }

    /// The mouse while the reorder modal is open: the widget drags or moves,
    /// and a confirmed order is written back to the field.
    fn handle_reorder_mouse(&mut self, mouse: MouseEvent, area: Rect, mut reorder: Reorder) {
        let outcome = reorder.handle_mouse(&mouse, area);
        self.settle_reorder(reorder, outcome);
    }

    /// Keys while the reorder modal is open.
    fn handle_reorder_key(&mut self, key: KeyEvent, mut reorder: Reorder) {
        let outcome = reorder.handle_key(&key);
        self.settle_reorder(reorder, outcome);
    }

    /// What the reorder modal decided: keep it open, keep the new order, or
    /// discard it.
    fn settle_reorder(&mut self, reorder: Reorder, outcome: ReorderOutcome) {
        match outcome {
            ReorderOutcome::Pending => self.reorder = Some(reorder),
            ReorderOutcome::Confirmed(order) => self.commit_reorder(order),
            ReorderOutcome::Cancelled => {}
        }
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
            // Take a provider out of this install. Only where one is under
            // the cursor: elsewhere `d` is nothing, not a delete.
            KeyCode::Char('d') | KeyCode::Delete if self.step == Step::Providers => {
                if let Some(index) = self.cursor_provider() {
                    self.remove_provider(index);
                }
            }
            // Straight to the add flow, from anywhere on the Providers screen.
            KeyCode::Char('a') if self.step == Step::Providers => self.open_add_provider(),
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
            // A provider row opens its setup modal; the row after the last
            // provider is the add flow.
            Step::Providers => match self.cursor_provider() {
                Some(index) => self.open_provider_modal(index),
                None => self.open_add_provider(),
            },
            Step::Agents | Step::Mcp => self.toggle(),
            Step::Defaults | Step::Limits => self.activate_field(),
            // Rowless steps put the cursor on their button, so these arms are
            // reachable only with a hand-forced cursor; acting on nothing is
            // correct then.
            Step::Welcome | Step::Review => {}
        }
        Action::Continue
    }

    /// Enter on the setup modal's card for the provider at `index`.
    ///
    /// Row 0 is the credential on every card that has one, and it opens its
    /// editor; `detail_action_at` answers `None` there. A browser sign-in has
    /// nothing to type, so its buttons start at row 0 instead and the `None`
    /// arm is only the cursor past the last of them.
    fn activate_detail_row(&mut self, index: usize) {
        match self.detail_action_at(index, self.cursor) {
            None => self.open_credential_editor(),
            Some(DetailAction::OpenSignup) => self.open_signup_page(),
            Some(DetailAction::SignIn) => self.request_signin(index, SigninAction::In),
            Some(DetailAction::SignOut) => self.request_signin(index, SigninAction::Out),
        }
    }

    /// Enter on an endpoint preset's screen (the preset at provider row
    /// `index`): a text field opens its editor, the default model cycles, the
    /// buttons do what they say, and the add row adds.
    fn activate_endpoint_row(&mut self, index: usize) {
        match self.endpoint_cursor(index) {
            Some(EndpointCursor::Add) => {
                self.add_endpoint(index);
            }
            Some(EndpointCursor::Field(entry, field)) if field.is_text() => {
                self.open_endpoint_editor(entry, field);
            }
            Some(EndpointCursor::Field(entry, EndpointField::DefaultModel)) => {
                self.cycle_endpoint_model(entry, 1);
            }
            Some(EndpointCursor::Field(entry, EndpointField::Verify)) => {
                self.request_endpoint_verification(entry);
                self.message = Some("Checking…".to_string());
            }
            Some(EndpointCursor::Field(entry, EndpointField::Remove)) => {
                self.remove_endpoint(entry);
                // The rows above the cursor are gone with the entry.
                self.cursor = self.cursor.min(self.row_count());
            }
            // Every field kind is matched above, so this is the cursor past
            // the rows: reachable only with a hand-forced cursor, and acting
            // on nothing is correct then.
            Some(EndpointCursor::Field(..)) | None => {}
        }
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
            FieldValue::Order(_) => {
                self.open_reorder();
                return;
            }
            FieldValue::Choice { options, index } => (options.clone(), *index),
        };
        // The list-valued fields are the two model choices at the end of the
        // tuning screen; the rest of that screen is numbers and switches, which
        // the arrows already handle well.
        self.open_picker(label, choice.0, choice.1);
    }

    /// Open the credential editor for the provider on screen. An endpoint
    /// preset never gets here: `activate` sends it to its own form first.
    fn open_credential_editor(&mut self) {
        let Some((index, credential, value)) = self.detail_row().map(|index| {
            let row = &self.providers[index];
            (index, row.provider.credential, row.value.clone())
        }) else {
            return;
        };
        self.edit = Some(Edit {
            target: EditTarget::Credential(index),
            line: LineEdit::new(value, credential == Credential::ApiKey),
        });
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

    /// `Space` (or Enter on a row): toggle whatever the cursor is on. On the
    /// Providers screen there is nothing to toggle: a provider is set up in
    /// its modal or taken out with `d`, so Space opens the modal the way
    /// Enter does.
    fn toggle(&mut self) {
        match self.step {
            Step::Providers => match self.cursor_provider() {
                Some(index) => self.open_provider_modal(index),
                None if self.on_add_provider() => self.open_add_provider(),
                None => {}
            },
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
            Step::Welcome | Step::Review => {}
        }
    }

    /// `←`/`→`: cycle a choice, or an endpoint entry's default model in the
    /// setup modal.
    fn adjust(&mut self, delta: isize) {
        // On an endpoint preset's card the default model cycles; everything
        // else there is typed.
        if let Some(index) = self.modal_index() {
            if self.is_endpoint_preset(index)
                && let Some(EndpointCursor::Field(entry, EndpointField::DefaultModel)) =
                    self.endpoint_cursor(index)
            {
                self.cycle_endpoint_model(entry, delta);
            }
            return;
        }
        match self.step {
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

    /// Advance, but not past the Providers screen with nothing configured:
    /// Leviath cannot run an agent without a provider, so the wizard will not
    /// write a config that has none.
    fn forward_guarded(&mut self) {
        if self.step == Step::Providers && self.selected_providers().is_empty() {
            self.message = Some("Add at least one provider to continue.".to_string());
            return;
        }
        self.forward();
    }

    /// `Tab`: next step.
    fn forward(&mut self) {
        self.next_step();
    }

    /// `Esc` / `Shift-Tab`: previous step.
    fn back(&mut self) {
        self.prev_step();
    }

    /// `v`: re-check every configured provider.
    fn verify_current(&mut self) {
        match self.step {
            Step::Providers | Step::Review => {
                if self.selected_providers().is_empty() {
                    self.message = Some("Nothing to check: no provider is configured.".to_string());
                    return;
                }
                self.verify_all();
                self.message = Some("Checking every configured provider…".to_string());
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
        // The modal's provider when one is open, else the provider under the
        // Providers screen's cursor.
        let row = match self.modal_index() {
            Some(index) => Some(index),
            None if self.step == Step::Providers => self.cursor_provider(),
            None => None,
        };
        let url = row
            .and_then(|i| self.providers.get(i))
            .and_then(|r| r.provider.signup_url);
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
