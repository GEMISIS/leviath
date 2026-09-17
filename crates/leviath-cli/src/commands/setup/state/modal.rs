//! A provider's setup, in a modal over the Providers screen.
//!
//! Adding a provider is three choices and a form: how it is reached (an API
//! key, a browser sign-in, a server you run), what it makes (text and images,
//! 3D models), which one, and then its credential. The three choices are the
//! shared chooser, one level at a time, with Esc stepping back a level; the
//! form is this modal: the provider's card, with three ways out at its foot:
//! verify and use, use without verifying, or cancel and put the provider back
//! the way it was.
//!
//! The modal drives the wizard's own cursor while it is open, so the card's
//! rows, the editor and the sign-in lane work inside it exactly as they did on
//! a screen; the Providers screen's cursor is kept aside and restored on close.

use super::*;

/// What Cancel puts back: the row, the endpoint entries under it, and where
/// the Providers screen's cursor was.
#[derive(Debug, Clone)]
struct Snapshot {
    row: ProviderRow,
    endpoints: Vec<EndpointRow>,
    cursor: usize,
    scroll: usize,
    /// Whether anything had changed before the modal opened: opening a
    /// preset's modal adds an entry, and cancelling must not leave the
    /// wizard believing that was a change.
    dirty: bool,
}

/// A provider's setup modal, open on the provider row at `index`.
#[derive(Debug, Clone)]
pub(crate) struct ProviderModal {
    /// The row of `providers` being set up.
    pub(crate) index: usize,
    /// "Verify and use" was pressed and the answer has not landed yet: the
    /// modal accepts itself when it does, and stays open if it says no.
    pub(crate) awaiting_verify: bool,
    snapshot: Snapshot,
}

/// A few words on each category, for the chooser's second column.
fn category_detail(category: &str) -> &'static str {
    match category {
        "API key" => "paste a key from the provider's console",
        "Subscription logins" => "sign in with your browser; billed to a subscription",
        _ => "a server you run, or an OpenAI-compatible endpoint",
    }
}

impl Wizard {
    /// The categories the catalog offers, in its order: how a provider is
    /// reached.
    pub(crate) fn provider_categories(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        for row in &self.providers {
            let category = row.provider.auth_kind();
            if !out.contains(&category) {
                out.push(category);
            }
        }
        out
    }

    /// The kinds offered under `category`: what its providers make.
    fn provider_kinds(&self, category: &str) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        for row in &self.providers {
            if row.provider.auth_kind() != category {
                continue;
            }
            for kind in row.provider.kinds() {
                if !out.contains(kind) {
                    out.push(kind);
                }
            }
        }
        out
    }

    /// The rows of `providers` under `category` that make `kind`.
    fn provider_rows_of(&self, category: &str, kind: &str) -> Vec<usize> {
        self.providers
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                row.provider.auth_kind() == category && row.provider.kinds().contains(&kind)
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// Every provider as a chooser option: its name, and what it does and how
    /// it is reached as the note a search also reads.
    fn provider_option(&self, index: usize) -> PickerOption {
        let row = &self.providers[index];
        let mut detail = format!(
            "{} ({}; {})",
            row.provider.blurb,
            row.provider.auth_kind(),
            row.provider.kinds().join(", ")
        );
        if row.selected {
            detail.push_str(" (already set up)");
        }
        PickerOption {
            value: row.provider.display.to_string(),
            detail,
        }
    }

    /// The providers the Providers screen lists: the ones this install has,
    /// in catalog order. Everything else is reachable through "Add a
    /// provider", so a first-time user meets one short list rather than the
    /// whole catalog.
    pub(crate) fn visible_providers(&self) -> Vec<usize> {
        self.selected_providers()
    }

    /// The provider row the Providers screen's cursor is on, if it is on one
    /// rather than on the add row or the button.
    pub(crate) fn cursor_provider(&self) -> Option<usize> {
        self.visible_providers().get(self.cursor).copied()
    }

    /// Whether the Providers screen's cursor is on its "Add a provider" row,
    /// which sits after the listed providers and before the button.
    pub(crate) fn on_add_provider(&self) -> bool {
        self.step == Step::Providers && self.screen_cursor() == self.visible_providers().len()
    }

    /// Start adding a provider: the chooser opens on the categories.
    pub(crate) fn open_add_provider(&mut self) {
        let options = self
            .provider_categories()
            .into_iter()
            .map(|category| PickerOption {
                value: category.to_string(),
                detail: category_detail(category).to_string(),
            })
            .collect();
        let every_provider = (0..self.providers.len())
            .map(|index| self.provider_option(index))
            .collect();
        self.picker_purpose = PickerPurpose::Category;
        self.picker = Some(
            Picker::new(
                "Add a provider",
                vec![
                    "How is the provider reached? Pick a category, then what it makes, then \
                     the provider itself. Or type a provider's name, or what it does, to go \
                     straight to it."
                        .to_string(),
                ],
                options,
                0,
            )
            .with_search_only(every_provider),
        );
    }

    /// The second level: what providers of `category` make.
    fn open_kind_picker(&mut self, category: &'static str) {
        let options = self
            .provider_kinds(category)
            .into_iter()
            .map(|kind| {
                let names: Vec<&str> = self
                    .provider_rows_of(category, kind)
                    .into_iter()
                    .map(|index| self.providers[index].provider.display)
                    .collect();
                PickerOption {
                    value: kind.to_string(),
                    detail: names.join(", "),
                }
            })
            .collect();
        self.picker_purpose = PickerPurpose::Kind { category };
        self.picker = Some(Picker::new(
            format!("Add a provider: {category}"),
            vec!["What should it make? Esc goes back to the categories.".to_string()],
            options,
            0,
        ));
    }

    /// The third level: the providers of `category` that make `kind`.
    fn open_provider_picker(&mut self, category: &'static str, kind: &'static str) {
        let rows = self.provider_rows_of(category, kind);
        let options = rows
            .iter()
            .map(|&index| {
                let row = &self.providers[index];
                let detail = match row.selected {
                    true => format!("{} (already set up)", row.provider.blurb),
                    false => row.provider.blurb.to_string(),
                };
                PickerOption {
                    value: row.provider.display.to_string(),
                    detail,
                }
            })
            .collect();
        self.picker_purpose = PickerPurpose::Provider {
            category,
            kind,
            rows,
        };
        self.picker = Some(Picker::new(
            format!("Add a provider: {kind}"),
            vec!["Which provider? Esc goes back a level.".to_string()],
            options,
            0,
        ));
    }

    /// The chooser chose `chosen` (an index into its options): a field's
    /// value is written, a level of the add flow opens the next one.
    pub(in crate::commands::setup) fn settle_picker_choice(&mut self, chosen: usize) {
        match self.picker_purpose.clone() {
            PickerPurpose::Field(_) => self.commit_picker(chosen),
            PickerPurpose::Category => {
                // The categories are listed first; a search finds the
                // providers after them, in catalog order.
                let categories = self.provider_categories();
                match categories.get(chosen).copied() {
                    Some(category) => self.open_kind_picker(category),
                    None => self.open_provider_modal(chosen - categories.len()),
                }
            }
            PickerPurpose::Kind { category } => {
                if let Some(kind) = self.provider_kinds(category).get(chosen).copied() {
                    self.open_provider_picker(category, kind);
                }
            }
            PickerPurpose::Provider { rows, .. } => {
                if let Some(&index) = rows.get(chosen) {
                    self.open_provider_modal(index);
                }
            }
        }
    }

    /// The chooser was dismissed: a level of the add flow goes back to the
    /// one before it, the top level and a field's chooser simply close.
    pub(in crate::commands::setup) fn picker_back(&mut self) {
        match self.picker_purpose.clone() {
            PickerPurpose::Field(_) | PickerPurpose::Category => {}
            PickerPurpose::Kind { .. } => self.open_add_provider(),
            PickerPurpose::Provider { category, .. } => self.open_kind_picker(category),
        }
    }

    /// Open the setup modal on the provider row at `index`.
    ///
    /// An endpoint preset with no entry yet gets one, so its form has
    /// something to show; Cancel takes it away again with everything else.
    pub(crate) fn open_provider_modal(&mut self, index: usize) {
        let Some(row) = self.providers.get(index).cloned() else {
            return;
        };
        let snapshot = Snapshot {
            endpoints: self
                .endpoints_under(row.provider.id)
                .into_iter()
                .map(|entry| self.endpoints[entry].clone())
                .collect(),
            row,
            cursor: self.cursor,
            scroll: self.scroll,
            dirty: self.dirty,
        };
        if self.is_endpoint_preset(index)
            && self
                .endpoints_under(self.providers[index].provider.id)
                .is_empty()
        {
            self.add_endpoint(index);
        }
        self.picker = None;
        self.edit = None;
        self.cursor = 0;
        self.scroll = 0;
        self.message = None;
        self.modal = Some(ProviderModal {
            index,
            awaiting_verify: false,
            snapshot,
        });
    }

    /// The provider row the open modal is set up on.
    pub(crate) fn modal_index(&self) -> Option<usize> {
        self.modal.as_ref().map(|m| m.index)
    }

    /// The cursor the screen underneath is drawn with.
    ///
    /// While a modal is open, `cursor` is the modal's, moving over its card
    /// and buttons. The Providers screen it covers keeps the cursor it had
    /// when the modal opened, so the keys that move through the modal leave
    /// the highlight underneath where the user put it.
    pub(crate) fn screen_cursor(&self) -> usize {
        self.modal
            .as_ref()
            .map_or(self.cursor, |modal| modal.snapshot.cursor)
    }

    /// The scroll offset the screen underneath is drawn with, on the same
    /// footing as [`Self::screen_cursor`].
    pub(crate) fn screen_scroll(&self) -> usize {
        self.modal
            .as_ref()
            .map_or(self.scroll, |modal| modal.snapshot.scroll)
    }

    /// How many cursor rows the modal's card has above its buttons: an
    /// endpoint preset's entry fields and add row, or the credential row and
    /// the actions.
    pub(crate) fn modal_card_rows(&self) -> usize {
        let Some(index) = self.modal_index() else {
            return 0;
        };
        match self.is_endpoint_preset(index) {
            true => self.endpoint_row_count(index),
            false => {
                usize::from(self.detail_has_credential_row(index)) + self.detail_actions().len()
            }
        }
    }

    /// Which of the modal's buttons `cursor` is on, if it is past the card.
    pub(crate) fn modal_button_at(&self, cursor: usize) -> Option<ModalButton> {
        self.modal.as_ref()?;
        ModalButton::ALL
            .get(cursor.checked_sub(self.modal_card_rows())?)
            .copied()
    }

    /// One of the modal's buttons was pressed.
    pub(in crate::commands::setup) fn activate_modal_button(&mut self, button: ModalButton) {
        let Some(index) = self.modal_index() else {
            return;
        };
        match button {
            ModalButton::VerifyUse => {
                self.request_verification(index);
                let in_flight = match self.is_endpoint_preset(index) {
                    true => self
                        .endpoints_under(self.providers[index].provider.id)
                        .into_iter()
                        .any(|entry| self.endpoints[entry].checking),
                    false => self.providers[index].checking,
                };
                if in_flight {
                    self.modal
                        .as_mut()
                        .expect("the modal is open: its index was read above")
                        .awaiting_verify = true;
                    self.message = Some("Checking…".to_string());
                } else {
                    self.message = Some(
                        "Nothing to check yet: enter a credential, or use it unverified."
                            .to_string(),
                    );
                }
            }
            ModalButton::SkipUse => self.accept_modal(),
            ModalButton::Cancel => self.cancel_modal(),
        }
    }

    /// Whether the modal's provider is set up enough to keep: a key where a
    /// key is typed, an entry where entries are, and nothing more for the
    /// kinds that carry nothing (a sign-in is usable once signed in, and
    /// choosing Ollama is the whole configuration).
    fn modal_can_accept(&self, index: usize) -> Result<(), &'static str> {
        let row = &self.providers[index];
        match row.provider.credential {
            Credential::ApiKey if !row.has_credential() => {
                Err("Enter an API key first, or cancel.")
            }
            Credential::Endpoint if self.endpoints_under(row.provider.id).is_empty() => {
                Err("Add an endpoint first, or cancel.")
            }
            _ => Ok(()),
        }
    }

    /// Keep the provider as it stands and close the modal.
    pub(crate) fn accept_modal(&mut self) {
        let Some(index) = self.modal_index() else {
            return;
        };
        if let Err(reason) = self.modal_can_accept(index) {
            self.message = Some(reason.to_string());
            return;
        }
        self.providers[index].selected = true;
        self.dirty = true;
        let display = self.providers[index].provider.display;
        let modal = self
            .modal
            .take()
            .expect("the modal is open: its index was read above");
        self.close_modal(modal);
        // The cursor lands on the provider just set up, so Enter reopens it.
        self.cursor = self
            .visible_providers()
            .iter()
            .position(|&row| row == index)
            .unwrap_or(0);
        self.message = Some(format!("{display} is set up."));
    }

    /// Put the provider back the way it was when the modal opened, and close.
    pub(crate) fn cancel_modal(&mut self) {
        let Some(modal) = self.modal.take() else {
            return;
        };
        let preset = modal.snapshot.row.provider.id;
        self.providers[modal.index] = modal.snapshot.row.clone();
        self.remove_endpoints_under(preset);
        self.endpoints
            .extend(modal.snapshot.endpoints.iter().cloned());
        self.dirty = modal.snapshot.dirty;
        self.close_modal(modal);
        self.message = Some("Cancelled; nothing changed.".to_string());
    }

    /// The Providers screen again, where the modal left it.
    fn close_modal(&mut self, modal: ProviderModal) {
        self.edit = None;
        self.cursor = modal.snapshot.cursor;
        self.scroll = modal.snapshot.scroll;
    }

    /// A verification asked for by "Verify and use" landed: keep the provider
    /// when every check passed, stay open with the answer on screen otherwise.
    pub(super) fn settle_modal_verification(&mut self) {
        let Some(index) = self.modal_index() else {
            return;
        };
        if !self.modal.as_ref().is_some_and(|m| m.awaiting_verify) {
            return;
        }
        let (in_flight, passed, failure) = match self.is_endpoint_preset(index) {
            true => {
                let entries = self.endpoints_under(self.providers[index].provider.id);
                let in_flight = entries.iter().any(|&e| self.endpoints[e].checking);
                let failed: Vec<&str> = entries
                    .iter()
                    .filter(|&&e| matches!(self.endpoints[e].outcome, Outcome::Failed { .. }))
                    .map(|&e| self.endpoints[e].name.as_str())
                    .collect();
                let failure = match failed.is_empty() {
                    true => None,
                    false => Some(format!("Check failed for {}.", failed.join(", "))),
                };
                (in_flight, failure.is_none(), failure)
            }
            false => {
                let row = &self.providers[index];
                let failure = match &row.outcome {
                    Outcome::Failed { .. } => Some(row.outcome.summary()),
                    _ => None,
                };
                let passed = matches!(row.outcome, Outcome::Reachable { .. });
                (row.checking, passed, failure)
            }
        };
        if in_flight {
            return;
        }
        self.modal
            .as_mut()
            .expect("the modal is open: its index was read above")
            .awaiting_verify = false;
        match (passed, failure) {
            (true, _) => self.accept_modal(),
            (false, Some(failure)) => self.message = Some(failure),
            (false, None) => {
                self.message = Some(
                    "Not checked: nothing to verify yet. Use it unverified, or cancel.".to_string(),
                )
            }
        }
    }

    /// Take the provider at `index` out of this install: its credential is
    /// cleared when the plan is applied, and its endpoint entries go with it.
    /// One supplied by the environment cannot be taken out from here, since
    /// the variable would put it straight back.
    pub(crate) fn remove_provider(&mut self, index: usize) {
        let Some(row) = self.providers.get(index) else {
            return;
        };
        if let Some(var) = row.from_env {
            self.message = Some(format!(
                "{} is supplied by ${var}; unset the variable to stop using it.",
                row.provider.display
            ));
            return;
        }
        let display = row.provider.display;
        let preset = row.provider.id;
        if self.is_endpoint_preset(index) {
            self.remove_endpoints_under(preset);
        }
        let row = &mut self.providers[index];
        row.selected = false;
        row.value.clear();
        row.outcome = Outcome::Skipped;
        row.checked_at = None;
        self.dirty = true;
        // One row fewer above the cursor: it stays where it was, or lands on
        // the add row when that is now the last row it could be on.
        self.cursor = self.cursor.min(self.visible_providers().len());
        self.message = Some(format!(
            "{display} removed; its credential is cleared when you finish."
        ));
    }
}
