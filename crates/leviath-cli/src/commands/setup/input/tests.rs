use super::*;
use crate::commands::setup::state::{
    ConfirmPurpose, DetailAction, Edit, EditTarget, FieldValue, ModalButton, PickerPurpose,
    SigninAction, VerifyReply,
};
use crate::commands::setup::verify::Outcome;

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

fn credential_edit(value: &str, masked: bool) -> Edit {
    Edit {
        target: EditTarget::Credential(0),
        line: LineEdit::new(value, masked),
    }
}

/// The row of `providers` for a catalog id.
fn provider(w: &Wizard, id: &str) -> usize {
    w.providers
        .iter()
        .position(|r| r.provider.id == id)
        .expect("the provider is in the catalog")
}

/// Type `text` into whatever is open, key by key.
fn type_str(w: &mut Wizard, text: &str) {
    for c in text.chars() {
        w.handle_key(press(KeyCode::Char(c)));
    }
}

/// Inside an open modal: open the credential editor, type `key`, commit.
fn type_credential(w: &mut Wizard, key: &str) {
    w.cursor = 0;
    w.handle_key(press(KeyCode::Enter));
    assert!(
        w.edit.is_some(),
        "Enter on the credential row opens the editor"
    );
    type_str(w, key);
    w.handle_key(press(KeyCode::Enter));
    assert!(w.edit.is_none(), "Enter commits the edit");
}

/// Inside an open modal: move onto "Skip verification and use" and press it.
fn press_skip_use(w: &mut Wizard) {
    w.cursor = w.modal_card_rows() + 1;
    assert_eq!(w.modal_button_at(w.cursor), Some(ModalButton::SkipUse));
    w.handle_key(press(KeyCode::Enter));
}

// ─── quitting and saving ────────────────────────────────────────────────

#[test]
fn ctrl_c_quits_immediately_when_nothing_was_changed() {
    let (_dir, mut w) = wizard();
    w.edit = Some(credential_edit("half-typed", true));

    let action = w.handle_key(press_with(KeyCode::Char('c'), KeyModifiers::CONTROL));

    assert_eq!(action, Action::Continue);
    assert!(w.should_quit);
}

#[test]
fn ctrl_c_with_unsaved_changes_asks_once_then_quits() {
    let (_dir, mut w) = wizard();
    w.dirty = true;

    w.handle_key(press_with(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(!w.should_quit, "first Ctrl-C asks");
    let pending = w.confirm.as_ref().expect("the quit dialog is open");
    assert_eq!(pending.purpose, ConfirmPurpose::QuitDiscard);
    assert!(!pending.dialog.focus_yes, "the safe answer holds focus");

    // A second Ctrl-C, with the dialog open, obeys unconditionally.
    w.handle_key(press_with(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(w.should_quit);
}

#[test]
fn q_quits_while_navigating_with_nothing_changed() {
    let (_dir, mut w) = wizard();
    w.handle_key(press(KeyCode::Char('q')));
    assert!(w.should_quit);
}

#[test]
fn q_with_unsaved_changes_opens_the_quit_dialog() {
    let (_dir, mut w) = wizard();
    w.dirty = true;

    w.handle_key(press(KeyCode::Char('q')));

    assert!(!w.should_quit);
    assert_eq!(
        w.confirm.as_ref().expect("dialog open").purpose,
        ConfirmPurpose::QuitDiscard
    );

    // Enter on the default (No / Stay) button closes the dialog and stays.
    w.handle_key(press(KeyCode::Enter));
    assert!(w.confirm.is_none());
    assert!(!w.should_quit);

    // Asking again and confirming with `y` quits.
    w.handle_key(press(KeyCode::Char('q')));
    w.handle_key(press(KeyCode::Char('y')));
    assert!(w.should_quit);
}

/// The dialog says what quitting throws away: the review's lines, verbatim.
#[test]
fn the_quit_dialog_lists_what_would_be_discarded() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.providers[0].value = "sk-ant".to_string();
    w.dirty = true;
    let changes = w.review_lines();
    assert!(
        changes.iter().any(|c| c.contains("Anthropic")),
        "the keyed provider is a pending change: {changes:?}"
    );

    w.handle_key(press(KeyCode::Char('q')));

    let pending = w.confirm.as_ref().expect("dialog open");
    let body: Vec<String> = pending
        .dialog
        .body
        .iter()
        .map(|line| line.spans.iter().map(|s| &*s.content).collect())
        .collect();
    // The dialog shows at most eight changes, then counts the rest.
    let shown = changes.len().min(8);
    let cut = changes.len() > 8;
    assert_eq!(body.len(), 1 + shown + usize::from(cut), "{body:?}");
    for (line, change) in body[1..=shown].iter().zip(&changes) {
        assert_eq!(line.trim(), change);
    }
    if cut {
        assert_eq!(
            body.last().map(|l| l.trim().to_string()).as_deref(),
            Some(format!("and {} more", changes.len() - 8).as_str())
        );
    }
}

/// More changes than the dialog can show are cut, with a count of the rest,
/// rather than pushing its buttons off the bottom.
#[test]
fn a_long_list_of_changes_is_cut_with_a_count() {
    let (_dir, mut w) = wizard();
    for row in &mut w.providers {
        if row.provider.credential == crate::commands::setup::catalog::Credential::ApiKey {
            row.selected = true;
            row.value = format!("key-for-{}", row.provider.id);
        }
    }
    for id in ["llama-cpp", "lm-studio", "openai-compatible"] {
        let index = provider(&w, id);
        w.add_endpoint(index);
    }
    w.limits[0].value = FieldValue::Number(Some(99));
    let changes = w.review_lines();
    assert!(changes.len() > 8, "enough changes to overflow: {changes:?}");

    w.handle_key(press(KeyCode::Char('q')));

    let pending = w.confirm.as_ref().expect("dialog open");
    let body: Vec<String> = pending
        .dialog
        .body
        .iter()
        .map(|line| line.spans.iter().map(|s| &*s.content).collect())
        .collect();
    assert_eq!(body.len(), 1 + 8 + 1, "{body:?}");
    assert_eq!(
        body.last().map(|l| l.trim().to_string()).as_deref(),
        Some(format!("and {} more", changes.len() - 8).as_str())
    );
}

#[test]
fn q_types_a_letter_while_editing_rather_than_quitting() {
    // Losing a half-entered API key to a quit shortcut would be a bad way
    // to find out about modal input.
    let (_dir, mut w) = wizard();
    w.edit = Some(credential_edit("", true));

    w.handle_key(press(KeyCode::Char('q')));

    assert!(!w.should_quit);
    assert_eq!(w.edit.as_ref().expect("still editing").line.value(), "q");
}

#[test]
fn ctrl_s_saves_from_any_screen_once_a_provider_is_configured() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Agents);

    let action = w.handle_key(press_with(KeyCode::Char('s'), KeyModifiers::CONTROL));

    assert_eq!(action, Action::Save);
}

/// A config with no provider cannot run an agent, so saving one is refused
/// and the wizard goes back to where a provider is added.
#[test]
fn ctrl_s_with_no_provider_sends_you_back_to_add_one() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Agents);

    let action = w.handle_key(press_with(KeyCode::Char('s'), KeyModifiers::CONTROL));

    assert_eq!(action, Action::Continue);
    assert_eq!(w.step, Step::Providers);
    assert_eq!(
        w.message.as_deref(),
        Some("Add at least one provider before finishing.")
    );
}

#[test]
fn enter_on_the_review_screen_saves() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Review);

    assert_eq!(w.handle_key(press(KeyCode::Enter)), Action::Save);
    assert!(w.confirm.is_none(), "no dialog gates the save");
}

#[test]
fn confirm_and_editor_guards_hold_when_driven_directly() {
    let (_dir, mut w) = wizard();

    // No dialog open: the confirm handler declines to act.
    assert_eq!(
        w.handle_confirm_key(press(KeyCode::Enter)),
        Action::Continue
    );
    assert!(w.confirm.is_none());

    // The field editor refuses a non-text (Bool) row.
    w.enter(Step::Limits);
    w.cursor = 3;
    assert!(!w.open_field_editor());

    // The credential editor refuses to open with no modal on screen.
    w.enter(Step::Providers);
    w.open_credential_editor();
    assert!(w.edit.is_none());
}

/// The modal's helpers are total: with no modal open they do nothing rather
/// than index a row that is not there.
#[test]
fn modal_helpers_with_no_modal_open_do_nothing() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);

    assert_eq!(w.modal_index(), None);
    assert_eq!(w.detail_row(), None);
    assert_eq!(w.modal_card_rows(), 0);
    assert_eq!(w.modal_button_at(0), None);
    assert!(w.detail_actions().is_empty());

    w.accept_modal();
    w.cancel_modal();
    w.activate_modal_button(ModalButton::SkipUse);
    w.activate_modal_button(ModalButton::VerifyUse);

    assert!(w.modal.is_none());
    assert!(w.message.is_none());
    assert!(!w.dirty);
    assert!(w.selected_providers().is_empty());
}

#[test]
fn a_direct_force_quit_action_sets_the_quit_flag() {
    // `handle_key` intercepts Ctrl-C before navigation; the nav arm still
    // behaves correctly when driven directly.
    let (_dir, mut w) = wizard();
    w.handle_nav_key(press_with(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(w.should_quit);
}

// ─── help overlay ───────────────────────────────────────────────────────

#[test]
fn the_help_overlay_opens_and_only_deliberate_keys_close_it() {
    let (_dir, mut w) = wizard();

    w.handle_key(press(KeyCode::Char('?')));
    assert!(w.show_help);

    // A random key is ignored: it neither closes help nor acts underneath.
    w.handle_key(press(KeyCode::Char('x')));
    assert!(w.show_help);

    // A dismissing key closes the overlay without also doing its normal job.
    w.handle_key(press(KeyCode::Char('q')));
    assert!(!w.show_help);
    assert!(!w.should_quit);
}

// ─── editing ────────────────────────────────────────────────────────────

#[test]
fn typing_backspacing_and_committing_a_credential() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);
    w.open_provider_modal(0);

    w.handle_key(press(KeyCode::Enter));
    assert!(w.edit.is_some(), "Enter opens the editor");
    type_str(&mut w, "sk-antX");
    w.handle_key(press(KeyCode::Backspace));
    w.handle_key(press(KeyCode::Enter));

    assert!(w.edit.is_none());
    assert_eq!(w.providers[0].value, "sk-ant");
    assert!(w.dirty, "a committed edit is an unsaved change");
    assert_eq!(w.modal_index(), Some(0), "the modal stays open to be used");
}

#[test]
fn escape_abandons_an_edit_without_changing_the_value() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.providers[0].value = "sk-ant-original".to_string();
    w.enter(Step::Providers);
    w.open_provider_modal(0);
    w.handle_key(press(KeyCode::Enter));
    w.handle_key(press(KeyCode::Char('z')));

    w.handle_key(press(KeyCode::Esc));

    assert!(w.edit.is_none());
    assert_eq!(w.providers[0].value, "sk-ant-original");
    assert_eq!(w.message.as_deref(), Some("Edit cancelled."));
    assert_eq!(
        w.modal_index(),
        Some(0),
        "cancelling the edit does not cancel the modal"
    );
}

#[test]
fn an_unhandled_key_while_editing_is_ignored() {
    let (_dir, mut w) = wizard();
    w.edit = Some(credential_edit("abc", false));

    w.handle_key(press(KeyCode::F(5)));

    assert_eq!(w.edit.as_ref().expect("still editing").line.value(), "abc");
}

#[test]
fn the_cursor_moves_within_an_edit() {
    // The shared LineEdit brings real cursor movement; prove it is wired.
    let (_dir, mut w) = wizard();
    w.edit = Some(credential_edit("ad", false));

    w.handle_key(press(KeyCode::Left));
    w.handle_key(press(KeyCode::Char('c')));

    assert_eq!(w.edit.as_ref().expect("still editing").line.value(), "acd");
}

#[test]
fn enter_on_a_toggle_flips_it_and_stays() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Limits);
    w.cursor = 3; // a boolean
    let before = w.limits[3].value.clone();

    w.handle_key(press(KeyCode::Enter));

    assert!(w.edit.is_none());
    assert_eq!(w.step, Step::Limits, "acting on a row never advances");
    assert_ne!(w.limits[3].value, before);
}

#[test]
fn the_arrows_still_cycle_a_choice_in_place() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Defaults);
    w.cursor = 0;
    // The provider field is an ordered priority now (Enter opens a modal), but
    // the arrow-cycle still applies to any plain choice, so drive one directly.
    w.defaults[0].value = FieldValue::Choice {
        options: vec!["anthropic".to_string(), "ollama".to_string()],
        index: 0,
    };

    w.handle_key(press(KeyCode::Right));

    assert_eq!(w.step, Step::Defaults);
    assert!(w.picker.is_none(), "an arrow is not a chooser");
    assert_eq!(w.defaults[0].value.display(), "ollama");
}

#[test]
fn enter_on_a_number_field_opens_it_seeded_with_the_current_value() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Limits);

    w.handle_key(press(KeyCode::Enter));

    let edit = w.edit.as_ref().expect("the editor opened");
    assert_eq!(edit.target, EditTarget::Field(0));
    assert!(
        !edit.line.value().is_empty(),
        "seeded from the current value"
    );
    assert!(!edit.line.masked, "a limit is not a secret");
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
            .line
            .value()
            .is_empty()
    );
}

#[test]
fn a_cursor_forced_out_of_range_acts_on_nothing() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Limits);
    w.cursor = 99;
    w.handle_key(press(KeyCode::Enter));
    assert!(w.edit.is_none());
    assert_eq!(w.step, Step::Limits);

    // The rowless steps have the same guard.
    for step in [Step::Welcome, Step::Review] {
        w.enter(step);
        w.cursor = 99;
        w.handle_key(press(KeyCode::Enter));
        assert_eq!(w.step, step);
    }
}

/// With nothing configured the Providers screen's first row is the add row,
/// and Enter or Space there starts the add flow rather than advancing.
#[test]
fn enter_or_space_on_the_add_row_starts_the_add_flow() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);
    assert!(
        w.on_add_provider(),
        "the add row is first when nothing is set up"
    );
    assert_eq!(w.cursor_provider(), None);

    w.handle_key(press(KeyCode::Enter));

    assert_eq!(w.step, Step::Providers);
    assert!(w.picker.is_some(), "the category chooser opened");
    assert_eq!(w.picker_purpose, PickerPurpose::Category);

    // Esc at the top level closes the chooser; Space opens it again.
    w.handle_key(press(KeyCode::Esc));
    assert!(w.picker.is_none());
    w.handle_key(press(KeyCode::Char(' ')));
    assert!(w.picker.is_some());
    assert_eq!(w.picker_purpose, PickerPurpose::Category);
}

// ─── selection ──────────────────────────────────────────────────────────

/// A provider is no longer toggled with Space: it is set up in its modal,
/// so Space on a provider row opens that modal, the same as Enter does.
#[test]
fn space_on_a_provider_row_opens_its_modal_rather_than_toggling() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Providers);

    w.handle_key(press(KeyCode::Char(' ')));

    assert_eq!(w.modal_index(), Some(0));
    assert!(
        w.providers[0].selected,
        "Space did not turn the provider off"
    );
    assert!(!w.dirty, "opening a modal changes nothing yet");
}

#[test]
fn enter_on_a_provider_row_opens_its_modal_rather_than_advancing() {
    // The old trap: users pressed Enter expecting to act on the row, and the
    // wizard advanced instead. Enter opens the row's setup modal.
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.providers[1].selected = true;
    w.enter(Step::Providers);
    w.cursor = 1;

    w.handle_key(press(KeyCode::Enter));

    assert_eq!(w.step, Step::Providers, "Enter on a row must not advance");
    assert_eq!(
        w.modal_index(),
        Some(1),
        "the modal opens on the row under the cursor"
    );
    assert_eq!(w.cursor, 0, "the modal starts on its first row");
}

#[test]
fn enter_on_agent_and_mcp_rows_toggles_like_space() {
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
        Default::default(),
    );

    w.enter(Step::Agents);
    let before = w.agents[0].selected;
    w.handle_key(press(KeyCode::Enter));
    assert_eq!(w.step, Step::Agents);
    assert_ne!(w.agents[0].selected, before);

    w.enter(Step::Mcp);
    w.handle_key(press(KeyCode::Enter));
    assert_eq!(w.step, Step::Mcp);
    assert!(!w.mcp[0].selected);
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
        Default::default(),
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
    for step in [Step::Welcome, Step::Review] {
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
    assert!(w.modal.is_none());
    assert!(w.picker.is_none());
}

/// `d` (or Delete) takes the provider under the cursor out of the install:
/// its credential goes, its check is forgotten, and the row leaves the list.
#[test]
fn d_removes_the_provider_under_the_cursor() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.providers[0].value = "sk-ant".to_string();
    w.providers[0].outcome = Outcome::Reachable { models: Vec::new() };
    w.providers[1].selected = true;
    w.enter(Step::Providers);
    assert_eq!(w.visible_providers(), vec![0, 1]);

    w.handle_key(press(KeyCode::Char('d')));

    assert!(!w.providers[0].selected);
    assert!(w.providers[0].value.is_empty());
    assert_eq!(w.providers[0].outcome, Outcome::Skipped);
    assert!(w.dirty, "a removal is an unsaved change");
    assert_eq!(
        w.message.as_deref(),
        Some("Anthropic removed; its credential is cleared when you finish.")
    );
    assert_eq!(w.visible_providers(), vec![1], "the row left the list");

    // Delete does the same for the row now under the cursor.
    w.handle_key(press(KeyCode::Delete));
    assert!(!w.providers[1].selected);
    assert!(w.visible_providers().is_empty());
    assert!(w.cursor <= w.row_count(), "the cursor stays on the screen");
}

/// `d` off a provider row (the add row, the button, another screen) is not a
/// delete of anything.
#[test]
fn d_where_no_provider_is_under_the_cursor_does_nothing() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Providers);
    w.cursor = 1; // the add row

    w.handle_key(press(KeyCode::Char('d')));
    assert!(w.providers[0].selected);
    assert!(!w.dirty);
    assert!(w.message.is_none());

    w.enter(Step::Agents);
    w.handle_key(press(KeyCode::Char('d')));
    assert!(w.providers[0].selected);
    assert!(!w.dirty);
}

/// A provider the environment supplies cannot be taken out from here: the
/// variable would put it straight back.
#[test]
fn d_refuses_a_provider_supplied_by_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = Wizard::new(
        crate::config::Config::default(),
        &|var| (var == "ANTHROPIC_API_KEY").then(|| "sk-env".to_string()),
        Vec::new(),
        Vec::new(),
        dir.path(),
        std::sync::Arc::new(|_| true),
        Default::default(),
    );
    assert_eq!(w.providers[0].from_env, Some("ANTHROPIC_API_KEY"));
    assert!(w.providers[0].selected);
    w.enter(Step::Providers);

    w.handle_key(press(KeyCode::Char('d')));

    assert!(w.providers[0].selected);
    assert!(!w.dirty);
    assert_eq!(
        w.message.as_deref(),
        Some("Anthropic is supplied by $ANTHROPIC_API_KEY; unset the variable to stop using it.")
    );

    // Its modal can still be used without typing: the environment's key
    // counts as a credential.
    w.handle_key(press(KeyCode::Enter));
    assert_eq!(w.modal_index(), Some(0));
    press_skip_use(&mut w);
    assert!(w.modal.is_none());
    assert_eq!(w.message.as_deref(), Some("Anthropic is set up."));
}

/// Removing an endpoint preset takes every entry under it away too.
#[test]
fn d_on_an_endpoint_preset_removes_its_entries() {
    let (_dir, mut w) = wizard();
    let lm_studio = provider(&w, "lm-studio");
    w.add_endpoint(lm_studio);
    w.add_endpoint(lm_studio);
    assert_eq!(w.endpoints.len(), 2);
    w.enter(Step::Providers);
    assert_eq!(w.cursor_provider(), Some(lm_studio));

    w.handle_key(press(KeyCode::Char('d')));

    assert!(w.endpoints.is_empty());
    assert!(!w.providers[lm_studio].selected);
    assert!(w.visible_providers().is_empty());
}

// ─── the Continue button ────────────────────────────────────────────────

#[test]
fn enter_on_the_continue_button_advances() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Providers);
    w.cursor = w.row_count(); // the button, after the provider and the add row
    assert_eq!(w.cursor, 2);
    assert!(w.on_continue());

    w.handle_key(press(KeyCode::Enter));

    assert_eq!(w.step, Step::Defaults);
}

#[test]
fn the_cursor_walks_past_the_last_row_onto_the_button() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Providers);
    let rows = w.row_count();
    for _ in 0..rows + 5 {
        w.handle_key(press(KeyCode::Down));
    }
    assert_eq!(w.cursor, rows, "clamped to the button, not past it");
    assert!(w.on_continue());
}

/// The wizard will not write a config with no provider, so the button stays
/// put and says why. No dialog: there is nothing to confirm past.
#[test]
fn continuing_with_no_providers_is_refused_with_a_message() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);
    w.cursor = w.row_count();
    assert!(w.on_continue());

    w.handle_key(press(KeyCode::Enter));

    assert_eq!(w.step, Step::Providers, "stays until a provider is added");
    assert!(w.confirm.is_none(), "a refusal, not a question");
    assert_eq!(
        w.message.as_deref(),
        Some("Add at least one provider to continue.")
    );
}

#[test]
fn tab_from_providers_with_none_configured_hits_the_same_guard() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);

    w.handle_key(press(KeyCode::Tab));

    assert_eq!(w.step, Step::Providers);
    assert!(w.confirm.is_none());
    assert_eq!(
        w.message.as_deref(),
        Some("Add at least one provider to continue.")
    );
}

#[test]
fn tab_with_a_provider_configured_advances_without_asking() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Providers);

    w.handle_key(press(KeyCode::Tab));

    assert!(w.confirm.is_none());
    assert!(w.message.is_none());
    assert_eq!(w.step, Step::Defaults);
}

// ─── movement ───────────────────────────────────────────────────────────

#[test]
fn both_arrow_and_vim_keys_move_the_cursor() {
    let (_dir, mut w) = wizard();
    for row in w.providers.iter_mut().take(3) {
        row.selected = true;
    }
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
    w.enter(Step::Defaults);
    w.cursor = 0;
    w.defaults[0].value = FieldValue::Choice {
        options: vec!["anthropic".to_string(), "ollama".to_string()],
        index: 0,
    };

    w.handle_key(press(KeyCode::Right));
    assert_eq!(w.defaults[0].value.display(), "ollama");
    w.handle_key(press(KeyCode::Char('h')));
    assert_eq!(w.defaults[0].value.display(), "anthropic");
    // Wrapping backwards from the first option lands on the last.
    w.handle_key(press(KeyCode::Left));
    assert_eq!(w.defaults[0].value.display(), "ollama");
}

#[test]
fn arrows_in_a_keyed_providers_modal_do_not_change_anything() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Providers);
    w.open_provider_modal(0);
    let before = w.providers[0].value.clone();

    w.handle_key(press(KeyCode::Right));
    w.handle_key(press(KeyCode::Left));

    assert_eq!(w.providers[0].value, before);
    assert!(w.edit.is_none());
    assert!(!w.dirty);
    assert_eq!(w.modal_index(), Some(0));
}

#[test]
fn arrows_on_a_non_choice_field_or_rowless_screen_are_harmless() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Limits);
    w.cursor = 0; // a number, not a choice
    w.handle_key(press(KeyCode::Right));
    assert_eq!(w.limits[0].value.display(), "8");

    w.enter(Step::Welcome);
    w.handle_key(press(KeyCode::Right));

    // The Providers list has nothing to cycle either.
    w.providers[0].selected = true;
    w.enter(Step::Providers);
    w.handle_key(press(KeyCode::Right));

    assert!(!w.should_quit);
    assert!(w.modal.is_none());
    assert!(!w.dirty);
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

/// Back from Agents skips the tuning screen, because it is off by default.
/// Turning it on puts it back in the path, in both directions.
#[test]
fn escape_goes_back_a_step_and_the_advanced_toggle_decides_which() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Agents);

    w.handle_key(press(KeyCode::Esc));
    assert_eq!(w.step, Step::Defaults);

    w.cursor = Wizard::ADVANCED_FIELD;
    w.handle_key(press(KeyCode::Char(' ')));
    assert!(w.show_advanced, "space on the toggle turns tuning on");

    w.handle_key(press(KeyCode::Tab));
    assert_eq!(w.step, Step::Limits);
    w.handle_key(press(KeyCode::Esc));
    assert_eq!(w.step, Step::Defaults);
}

/// Tab and Shift-Tab inside the modal move over its rows; they never leave
/// the screen underneath, which is what they do everywhere else.
#[test]
fn tab_inside_the_modal_moves_the_cursor_rather_than_leaving() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);
    w.open_provider_modal(0);

    w.handle_key(press(KeyCode::Tab));
    assert_eq!(w.cursor, 1);
    assert_eq!(w.modal_index(), Some(0));
    assert_eq!(w.step, Step::Providers);

    w.handle_key(press(KeyCode::BackTab));
    assert_eq!(w.cursor, 0);
    assert_eq!(w.modal_index(), Some(0));
}

/// Esc in the modal is Cancel: the row, its entries and the Providers cursor
/// go back to how they were when the modal opened.
#[test]
fn escape_in_the_modal_cancels_and_restores_the_row() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.providers[0].value = "sk-ant-original".to_string();
    w.providers[1].selected = true;
    w.enter(Step::Providers);
    w.cursor = 1;

    w.handle_key(press(KeyCode::Enter));
    assert_eq!(w.modal_index(), Some(1));
    type_credential(&mut w, "sk-oai-typed");
    assert_eq!(w.providers[1].value, "sk-oai-typed");

    w.handle_key(press(KeyCode::Esc));

    assert!(w.modal.is_none());
    assert_eq!(w.step, Step::Providers);
    assert_eq!(w.providers[1].value, "", "the typed key is put back");
    assert!(w.providers[1].selected, "and so is the selection");
    assert_eq!(
        w.providers[0].value, "sk-ant-original",
        "the other row is untouched"
    );
    assert_eq!(w.cursor, 1, "the Providers cursor is where it was");
    assert_eq!(w.message.as_deref(), Some("Cancelled; nothing changed."));
}

/// The cursor walks the card's rows and then the three buttons, and stops
/// on the last of them.
#[test]
fn the_cursor_walks_the_modal_onto_its_buttons() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);
    w.open_provider_modal(0);
    assert_eq!(
        w.detail_actions(),
        vec![DetailAction::OpenSignup],
        "a keyed provider offers its key page, and no separate check"
    );
    assert_eq!(
        w.modal_card_rows(),
        2,
        "the credential row and the key page"
    );
    assert_eq!(w.row_count(), 2);
    assert_eq!(w.nav_rows(), 5, "the card's rows plus three buttons");
    assert_eq!(w.modal_button_at(1), None);
    assert_eq!(w.modal_button_at(2), Some(ModalButton::VerifyUse));
    assert_eq!(w.modal_button_at(3), Some(ModalButton::SkipUse));
    assert_eq!(w.modal_button_at(4), Some(ModalButton::Cancel));
    assert_eq!(w.modal_button_at(5), None);
    assert_eq!(
        ModalButton::ALL.map(ModalButton::label),
        ["Verify and use", "Skip verification and use", "Cancel"]
    );

    for _ in 0..10 {
        w.handle_key(press(KeyCode::Down));
    }
    assert_eq!(w.cursor, 4, "clamped to the last button");
    assert!(!w.on_continue(), "the modal's buttons are its own");
    w.handle_key(press(KeyCode::Up));
    assert_eq!(w.cursor, 3, "up walks back onto the button above");
    w.handle_key(press(KeyCode::Home));
    assert_eq!(w.cursor, 0, "home jumps to the top of the card");
    w.handle_key(press(KeyCode::PageDown));
    assert_eq!(w.cursor, 4, "a page jump clamps to the last button");
    w.handle_key(press(KeyCode::PageUp));
    assert_eq!(w.cursor, 0);
    w.handle_key(press(KeyCode::End));
    assert_eq!(w.cursor, 4, "end jumps to the last button");

    // Enter on Cancel closes it the way Esc does.
    w.handle_key(press(KeyCode::Enter));
    assert!(w.modal.is_none());
    assert_eq!(w.message.as_deref(), Some("Cancelled; nothing changed."));
}

/// "Skip verification and use" keeps the provider as typed, marks it
/// configured, and lands the Providers cursor on it.
#[test]
fn skip_verification_and_use_keeps_the_provider_and_closes() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Providers);
    let openai = provider(&w, "openai");
    w.open_provider_modal(openai);
    type_credential(&mut w, "sk-oai");

    press_skip_use(&mut w);

    assert!(w.modal.is_none());
    assert!(w.providers[openai].selected);
    assert_eq!(w.providers[openai].value, "sk-oai");
    assert!(w.dirty);
    assert_eq!(w.message.as_deref(), Some("OpenAI is set up."));
    assert_eq!(w.visible_providers(), vec![0, openai]);
    assert_eq!(w.cursor, 1, "the cursor lands on the provider just set up");
    assert_eq!(w.step, Step::Providers);
}

/// An API-key provider with no key, typed or in the environment, cannot be
/// used: the modal stays open and says what is missing.
#[test]
fn skip_verification_refuses_an_api_key_provider_with_no_key() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);
    w.open_provider_modal(0);

    press_skip_use(&mut w);

    assert_eq!(w.modal_index(), Some(0), "still open");
    assert!(!w.providers[0].selected);
    assert!(!w.dirty);
    assert_eq!(
        w.message.as_deref(),
        Some("Enter an API key first, or cancel.")
    );

    // With a key typed the same button accepts.
    type_credential(&mut w, "sk-ant");
    press_skip_use(&mut w);
    assert!(w.modal.is_none());
    assert!(w.providers[0].selected);
    assert_eq!(w.message.as_deref(), Some("Anthropic is set up."));
}

/// Ollama carries nothing to type: choosing it is the whole configuration,
/// so its modal accepts as it stands.
#[test]
fn a_provider_with_nothing_to_type_is_accepted_as_it_stands() {
    let (_dir, mut w) = wizard();
    let ollama = provider(&w, "ollama");
    w.enter(Step::Providers);
    w.open_provider_modal(ollama);

    press_skip_use(&mut w);

    assert!(w.modal.is_none());
    assert!(w.providers[ollama].selected);
    assert!(w.dirty);
    assert_eq!(w.visible_providers(), vec![ollama]);
}

/// Ctrl-S is refused while a provider is half set up: the modal has to be
/// used or cancelled first.
#[test]
fn ctrl_s_in_the_modal_is_refused() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Providers);
    w.open_provider_modal(0);

    let action = w.handle_key(press_with(KeyCode::Char('s'), KeyModifiers::CONTROL));

    assert_eq!(action, Action::Continue);
    assert_eq!(w.modal_index(), Some(0));
    assert_eq!(
        w.message.as_deref(),
        Some("Finish the provider first: use it, or cancel.")
    );
}

/// `q` in the modal asks the same question it asks anywhere else.
#[test]
fn q_in_the_modal_asks_to_quit_when_there_are_changes() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);
    w.open_provider_modal(0);
    w.dirty = true;

    w.handle_key(press(KeyCode::Char('q')));

    assert!(!w.should_quit);
    assert_eq!(
        w.confirm.as_ref().expect("dialog open").purpose,
        ConfirmPurpose::QuitDiscard
    );
    assert_eq!(
        w.modal_index(),
        Some(0),
        "the modal is still there underneath"
    );

    w.handle_key(press(KeyCode::Esc));
    assert!(w.confirm.is_none());
    w.dirty = false;
    w.handle_key(press(KeyCode::Char('q')));
    assert!(w.should_quit, "with nothing changed, q quits");
}

/// The Providers screen's own keys mean nothing inside the modal.
#[test]
fn the_modal_ignores_the_list_screens_keys() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Providers);
    w.open_provider_modal(0);

    w.handle_key(press(KeyCode::Char('a')));
    assert!(w.picker.is_none(), "a is not the add flow here");
    w.handle_key(press(KeyCode::Char('d')));
    assert!(w.providers[0].selected, "d is not a removal here");
    w.handle_key(press(KeyCode::F(9)));

    assert_eq!(w.modal_index(), Some(0));
    assert!(!w.dirty);
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
fn ctrl_r_reveals_even_while_typing_a_credential() {
    // Revealing is most useful mid-typo; the chord must not be eaten (or
    // inserted as a literal 'r') by the open editor.
    let (_dir, mut w) = wizard();
    w.edit = Some(credential_edit("sk-", true));

    w.handle_key(press_with(KeyCode::Char('r'), KeyModifiers::CONTROL));

    assert!(w.reveal);
    assert_eq!(w.edit.as_ref().expect("still editing").line.value(), "sk-");
}

/// `v` in the modal is "Verify and use": the check goes out, and the reply
/// decides. A pass keeps the provider and closes the modal.
#[test]
fn v_in_the_modal_verifies_and_uses_the_provider_when_the_check_passes() {
    let (_dir, mut w) = wizard();
    let (mut requests, replies) = w.take_verify_ends().expect("first take");
    w.providers[0].value = "sk-ant".to_string();
    w.enter(Step::Providers);
    w.open_provider_modal(0);

    w.handle_key(press(KeyCode::Char('v')));

    assert_eq!(w.message.as_deref(), Some("Checking…"));
    assert_eq!(
        requests.try_recv().expect("queued").provider_id,
        "anthropic"
    );
    assert!(w.providers[0].checking);
    assert!(
        w.modal.as_ref().is_some_and(|m| m.awaiting_verify),
        "the modal waits for exactly this answer"
    );
    assert!(
        !w.providers[0].selected,
        "nothing is kept until the answer lands"
    );

    replies
        .send(VerifyReply {
            provider_id: "anthropic".to_string(),
            outcome: Outcome::Reachable {
                models: vec!["claude-sonnet-4-6".to_string()],
            },
        })
        .expect("the wizard holds the receiver");
    w.drain_verifications();

    assert!(w.modal.is_none(), "a pass accepts the modal");
    assert!(w.providers[0].selected);
    assert!(!w.providers[0].checking);
    assert!(w.dirty);
    assert_eq!(w.message.as_deref(), Some("Anthropic is set up."));
    assert_eq!(w.step, Step::Providers);
}

/// A failed check keeps the modal open with the failure on screen, so the
/// key can be fixed or the provider used unverified.
#[test]
fn a_failed_verify_and_use_keeps_the_modal_open_with_the_reason() {
    let (_dir, mut w) = wizard();
    let (_requests, replies) = w.take_verify_ends().expect("first take");
    w.providers[0].value = "sk-ant".to_string();
    w.enter(Step::Providers);
    w.open_provider_modal(0);
    w.handle_key(press(KeyCode::Char('v')));

    replies
        .send(VerifyReply {
            provider_id: "anthropic".to_string(),
            outcome: Outcome::Failed {
                message: "401 unauthorized".to_string(),
            },
        })
        .expect("the wizard holds the receiver");
    w.drain_verifications();

    assert_eq!(w.modal_index(), Some(0), "still open");
    assert!(
        w.modal.as_ref().is_some_and(|m| !m.awaiting_verify),
        "no longer waiting"
    );
    assert!(!w.providers[0].selected);
    assert_eq!(w.message.as_deref(), Some("401 unauthorized"));

    // The provider can still be used unverified from here.
    press_skip_use(&mut w);
    assert!(w.modal.is_none());
    assert!(w.providers[0].selected);
}

/// `v` with nothing to check sends nothing and says so, rather than waiting
/// on an answer that will never come.
#[test]
fn v_in_the_modal_with_nothing_to_check_says_so() {
    let (_dir, mut w) = wizard();
    let (mut requests, _replies) = w.take_verify_ends().expect("first take");
    w.enter(Step::Providers);
    w.open_provider_modal(0);

    w.handle_key(press(KeyCode::Char('v')));

    assert!(requests.try_recv().is_err(), "nothing was queued");
    assert!(w.modal.as_ref().is_some_and(|m| !m.awaiting_verify));
    assert_eq!(
        w.message.as_deref(),
        Some("Nothing to check yet: enter a credential, or use it unverified.")
    );
}

/// Enter on the "Verify and use" button is the same as `v`.
#[test]
fn enter_on_the_verify_button_sends_the_check() {
    let (_dir, mut w) = wizard();
    let (mut requests, _replies) = w.take_verify_ends().expect("first take");
    w.providers[0].value = "sk-ant".to_string();
    w.enter(Step::Providers);
    w.open_provider_modal(0);
    w.cursor = w.modal_card_rows();
    assert_eq!(w.modal_button_at(w.cursor), Some(ModalButton::VerifyUse));

    w.handle_key(press(KeyCode::Enter));

    assert_eq!(w.message.as_deref(), Some("Checking…"));
    assert!(requests.try_recv().is_ok());
    assert!(w.modal.as_ref().is_some_and(|m| m.awaiting_verify));
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
    assert_eq!(
        w.message.as_deref(),
        Some("Checking every configured provider…")
    );

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
    assert!(w.message.is_none());
}

#[test]
fn v_on_the_list_with_nothing_configured_sends_nothing() {
    let (_dir, mut w) = wizard();
    let (mut requests, _replies) = w.take_verify_ends().expect("first take");
    w.enter(Step::Providers);

    w.handle_key(press(KeyCode::Char('v')));

    assert!(requests.try_recv().is_err());
    assert!(w.modal.is_none());
}

/// `o` opens the page of the provider under the list's cursor, or of the
/// modal's provider while one is open.
#[test]
fn o_opens_the_signup_page_from_the_list_and_from_the_modal() {
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
        Default::default(),
    );

    w.providers[0].selected = true;
    w.enter(Step::Providers);
    w.handle_key(press(KeyCode::Char('o')));

    let openai = provider(&w, "openai");
    w.open_provider_modal(openai);
    w.handle_key(press(KeyCode::Char('o')));

    let urls = opened.lock().expect("not poisoned").clone();
    assert_eq!(urls.len(), 2);
    assert!(urls.iter().all(|u| u.starts_with("https://")));
    assert_ne!(urls[0], urls[1], "the modal's provider, not the list's");
    assert!(
        w.message
            .as_deref()
            .unwrap_or_default()
            .starts_with("Opened"),
        "the user is told which page opened"
    );
    assert_eq!(
        w.modal_index(),
        Some(openai),
        "opening a page keeps the modal"
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
        Default::default(),
    );
    w.providers[0].selected = true;
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
        .position(|r| r.provider.signup_url.is_none())
        .expect("a provider with nowhere to go");
    w.enter(Step::Providers);
    w.open_provider_modal(index);
    w.handle_key(press(KeyCode::Char('o')));
    assert_eq!(w.message.as_deref(), Some("Nothing to open here."));
    w.cancel_modal();

    // The add row is not a provider.
    w.message = None;
    assert!(w.on_add_provider());
    w.handle_key(press(KeyCode::Char('o')));
    assert_eq!(w.message.as_deref(), Some("Nothing to open here."));

    w.enter(Step::Limits);
    w.message = None;
    w.handle_key(press(KeyCode::Char('o')));
    assert_eq!(w.message.as_deref(), Some("Nothing to open here."));
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

// ─── dialogs and saving ─────────────────────────────────────────────────

#[test]
fn a_dialog_holds_focus_against_stray_keys() {
    let (_dir, mut w) = wizard();
    w.dirty = true;
    w.enter(Step::Review);
    w.handle_key(press(KeyCode::Char('q')));
    assert!(w.confirm.is_some());

    // A second q neither quits nor dismisses: a stray key never answers a
    // dialog.
    w.handle_key(press(KeyCode::Char('q')));
    assert!(!w.should_quit, "q must not quit while a dialog is open");
    assert!(w.confirm.is_some(), "and must not dismiss it either");

    // Esc explicitly declines.
    w.handle_key(press(KeyCode::Esc));
    assert!(w.confirm.is_none());
    assert!(!w.should_quit);
}

/// No dialog gates the save; with no provider it is refused outright and the
/// wizard goes back to the Providers screen to add one.
#[test]
fn enter_on_review_with_no_provider_goes_back_to_add_one() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Review);

    let action = w.handle_key(press(KeyCode::Enter));

    assert_eq!(action, Action::Continue);
    assert!(w.confirm.is_none());
    assert_eq!(w.step, Step::Providers);
    assert_eq!(
        w.message.as_deref(),
        Some("Add at least one provider before finishing.")
    );
}

// ─── the mouse ──────────────────────────────────────────────────────────

/// The window the click tests aim at.
const AREA: Rect = Rect::new(0, 0, 90, 40);

fn click(column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::empty(),
    }
}

fn wheel(down: bool) -> MouseEvent {
    MouseEvent {
        kind: if down {
            MouseEventKind::ScrollDown
        } else {
            MouseEventKind::ScrollUp
        },
        column: 4,
        row: 6,
        modifiers: KeyModifiers::empty(),
    }
}

/// Where a given row is drawn, so the assertions below say which row they
/// clicked rather than encoding a line layout that will move.
fn point_of_row(w: &Wizard, row: usize) -> (u16, u16) {
    for y in 0..AREA.height {
        if crate::commands::setup::render::row_at(AREA, w, 4, y) == Some(row) {
            return (4, y);
        }
    }
    panic!("row {row} is not on screen");
}

#[test]
fn clicking_a_provider_row_opens_its_modal() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.providers[1].selected = true;
    w.enter(Step::Providers);
    let (x, y) = point_of_row(&w, 1);

    w.handle_mouse(click(x, y), AREA);

    assert_eq!(w.modal_index(), Some(1), "the click acts on what it hit");
    assert_eq!(w.cursor, 0, "the modal's own cursor starts at its top");
}

#[test]
fn clicking_the_add_row_starts_the_add_flow() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Providers);
    let (x, y) = point_of_row(&w, 1);

    w.handle_mouse(click(x, y), AREA);

    assert!(w.picker.is_some());
    assert_eq!(w.picker_purpose, PickerPurpose::Category);
    assert!(w.modal.is_none());
}

#[test]
fn clicking_the_button_advances_the_step() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Agents);
    let button = w.nav_rows() - 1;
    let (x, y) = point_of_row(&w, button);

    w.handle_mouse(click(x, y), AREA);

    assert_ne!(w.step, Step::Agents, "the button is a button when clicked");
}

/// The key page is a row of the modal's card, reachable with the arrows and
/// Enter, not a shortcut key alone.
#[test]
fn the_modal_offers_the_key_page_as_a_row() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);
    w.open_provider_modal(0);
    assert_eq!(w.detail_actions(), vec![DetailAction::OpenSignup]);

    w.cursor = 1;
    w.handle_key(press(KeyCode::Enter));

    assert!(w.edit.is_none(), "the action row is not a field");
    assert!(
        w.message
            .as_deref()
            .is_some_and(|m| m.starts_with("Opened")),
        "Enter on the key-page row opens the page: {:?}",
        w.message
    );
    assert_eq!(w.modal_index(), Some(0));
}

/// A provider with nowhere to sign up has only its credential row above the
/// buttons: the check is a button now, so nothing is left to offer as a row.
/// Every keyed row in the catalog has a key page today, so the test strips one.
#[test]
fn a_provider_without_a_key_page_offers_no_action_rows() {
    let (_dir, mut w) = wizard();
    w.providers[0].provider.signup_url = None;
    w.enter(Step::Providers);
    w.open_provider_modal(0);

    assert!(w.detail_actions().is_empty());
    assert_eq!(w.modal_card_rows(), 1, "the credential row alone");
    assert_eq!(w.row_count(), 1);
    assert_eq!(w.modal_button_at(1), Some(ModalButton::VerifyUse));
}

/// The sign-in card's buttons start at row 0, and pressing the plans-page row
/// must not open a text editor: there is no credential behind it to edit.
#[test]
fn a_sign_in_modal_has_no_credential_to_edit() {
    let (_dir, mut w) = wizard();
    let index = provider(&w, "codex");
    for row in &mut w.providers {
        row.selected = false;
        // See `render::tests::codex_card`: the ambient grant store is shared
        // with whatever else is running.
        row.signed_in = None;
    }
    w.enter(Step::Providers);
    w.open_provider_modal(index);
    assert!(!w.detail_has_credential_row(index));
    assert_eq!(
        w.detail_actions(),
        vec![DetailAction::SignIn, DetailAction::OpenSignup]
    );
    assert_eq!(w.modal_card_rows(), 2);

    // Row 1 is the plans-page button, not a field. (Row 0 is the sign-in,
    // which would ask the lane rather than opening an editor either way.)
    w.cursor = 1;
    w.handle_key(press(KeyCode::Enter));
    assert!(w.edit.is_none(), "an editor opened over nothing to type");
    assert!(
        w.message
            .as_deref()
            .is_some_and(|m| m.starts_with("Opened")),
        "{:?}",
        w.message
    );
}

/// Enter on the sign-in button asks the lane rather than doing it inline, so
/// the wizard keeps drawing while the browser is open.
#[test]
fn the_sign_in_button_asks_the_lane() {
    let (_dir, mut w) = wizard();
    let index = provider(&w, "codex");
    for row in &mut w.providers {
        row.selected = false;
        // See `render::tests::codex_card`: the ambient grant store is shared
        // with whatever else is running.
        row.signed_in = None;
    }
    w.enter(Step::Providers);
    w.open_provider_modal(index);
    let (mut requests, _events) = w.take_signin_ends().expect("first take");

    w.cursor = 0;
    w.handle_key(press(KeyCode::Enter));

    let request = requests.try_recv().expect("the lane was asked");
    assert_eq!(request.provider_id, "codex");
    assert!(w.providers[index].signing_in);
    assert_eq!(
        w.modal_index(),
        Some(index),
        "the modal waits with the browser"
    );

    // And with a sign-in stored the rows shift: sign in again, then sign
    // out, which is the second, then the plans page.
    w.providers[index].signed_in = Some("a@b.c".to_string());
    w.providers[index].signing_in = false;
    assert_eq!(
        w.detail_actions(),
        vec![
            DetailAction::SignIn,
            DetailAction::SignOut,
            DetailAction::OpenSignup
        ]
    );
    w.cursor = 1;
    w.handle_key(press(KeyCode::Enter));
    let request = requests.try_recv().expect("the lane was asked again");
    assert_eq!(request.action, SigninAction::Out);

    // Signed in, the modal accepts without anything typed.
    press_skip_use(&mut w);
    assert!(w.modal.is_none());
    assert!(w.providers[index].selected);
}

#[test]
fn a_click_outside_the_body_does_nothing() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Providers);

    // The footer, which is not a row and must not be treated as the nearest one.
    w.handle_mouse(click(4, AREA.height - 1), AREA);

    assert_eq!(w.cursor, 0);
    assert!(w.modal.is_none());
    assert!(w.picker.is_none());
}

#[test]
fn the_wheel_moves_the_selection_so_the_view_follows_it() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Providers);

    w.handle_mouse(wheel(true), AREA);
    assert_eq!(w.cursor, 1);
    w.handle_mouse(wheel(false), AREA);
    assert_eq!(w.cursor, 0);
}

/// A click cannot mean anything while a dialog, an edit or the setup modal is
/// up: taking it as a dismissal would throw away a half-typed credential, and
/// a click through the modal would land on the list underneath.
#[test]
fn clicks_are_ignored_while_a_dialog_an_edit_or_a_modal_is_open() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Providers);
    let (x, y) = point_of_row(&w, 0);

    w.edit = Some(credential_edit("half-typed", true));
    w.handle_mouse(click(x, y), AREA);
    assert!(w.edit.is_some(), "the edit survives a stray click");
    assert!(w.modal.is_none());

    w.edit = None;
    w.open_quit_confirm();
    w.handle_mouse(click(x, y), AREA);
    assert!(w.confirm.is_some());
    assert!(w.modal.is_none());

    w.confirm = None;
    w.open_provider_modal(0);
    w.cursor = 1;
    w.handle_mouse(click(x, y), AREA);
    w.handle_mouse(wheel(true), AREA);
    assert_eq!(w.modal_index(), Some(0), "the modal is keyboard-driven");
    assert_eq!(w.cursor, 1, "and the mouse does not move inside it");
    assert!(w.message.is_none());
}

// ─── adding a provider ──────────────────────────────────────────────────

/// What the add flow's last level lists after "API key" then "Text and
/// images": the rows of `providers` under that category and kind.
fn keyed_text_providers(w: &Wizard) -> PickerPurpose {
    PickerPurpose::Provider {
        category: "API key",
        kind: "Text and images",
        rows: w
            .providers
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                r.provider.auth_kind() == "API key"
                    && r.provider.kinds().contains(&"Text and images")
            })
            .map(|(index, _)| index)
            .collect(),
    }
}

/// `a` walks the chooser through category, kind and provider, and the last
/// choice opens that provider's modal.
#[test]
fn the_add_flow_walks_category_kind_and_provider_then_opens_the_modal() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);

    w.handle_key(press(KeyCode::Char('a')));

    let picker = w.picker.as_ref().expect("the category chooser is open");
    assert_eq!(picker.title, "Add a provider");
    assert_eq!(w.picker_purpose, PickerPurpose::Category);
    // The list is the categories; the providers after them are there for a
    // search only.
    let categories: Vec<&str> = picker
        .matches()
        .into_iter()
        .map(|i| picker.options[i].value.as_str())
        .collect();
    assert_eq!(
        categories,
        ["API key", "Subscription logins", "Local and custom"]
    );
    assert_eq!(w.provider_categories(), categories);
    assert!(
        picker.options.iter().all(|o| !o.detail.is_empty()),
        "each category and provider says what it means"
    );

    // Choosing "API key" opens the kinds it offers.
    w.handle_key(press(KeyCode::Enter));
    let picker = w.picker.as_ref().expect("the kind chooser is open");
    assert_eq!(
        w.picker_purpose,
        PickerPurpose::Kind {
            category: "API key"
        }
    );
    let kinds: Vec<&str> = picker.options.iter().map(|o| o.value.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "Text and images",
            "Video",
            "Speech and audio",
            "3D models and textures"
        ]
    );
    assert!(
        picker.options[0].detail.contains("Anthropic"),
        "a kind names the providers under it: {}",
        picker.options[0].detail
    );

    // Choosing "Text and images" lists the keyed text providers by name.
    w.handle_key(press(KeyCode::Enter));
    let picker = w.picker.as_ref().expect("the provider chooser is open");
    assert_eq!(w.picker_purpose, keyed_text_providers(&w));
    assert_eq!(picker.title, "Add a provider: Text and images");
    assert_eq!(
        picker.options[0].value, "Anthropic",
        "the first row is Anthropic"
    );
    assert!(
        picker
            .options
            .iter()
            .all(|o| !o.value.is_empty() && !o.detail.is_empty()),
        "each provider is listed by name with its blurb"
    );
    assert!(
        picker
            .options
            .iter()
            .all(|o| !o.detail.ends_with("(already set up)")),
        "nothing is configured yet"
    );

    // Choosing Anthropic opens its modal.
    w.handle_key(press(KeyCode::Enter));
    assert!(w.picker.is_none());
    assert_eq!(w.modal_index(), Some(0));
    assert_eq!(w.step, Step::Providers);
}

/// Typing in the category chooser finds a provider by its name or what it
/// does, and choosing it opens its modal without the levels between.
#[test]
fn typing_in_the_add_flow_finds_a_provider_by_name_or_description() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);
    w.handle_key(press(KeyCode::Char('a')));
    for c in "anthropic".chars() {
        w.handle_key(press(KeyCode::Char(c)));
    }
    let picker = w.picker.as_ref().expect("still choosing");
    let found: Vec<&str> = picker
        .matches()
        .into_iter()
        .map(|i| picker.options[i].value.as_str())
        .collect();
    assert_eq!(found, ["Anthropic"]);
    w.handle_key(press(KeyCode::Enter));
    assert!(w.picker.is_none());
    let anthropic = w
        .providers
        .iter()
        .position(|r| r.provider.id == "anthropic")
        .unwrap();
    assert_eq!(w.modal_index(), Some(anthropic));

    w.cancel_modal();
    w.handle_key(press(KeyCode::Char('a')));
    for c in "sora".chars() {
        w.handle_key(press(KeyCode::Char(c)));
    }
    let picker = w.picker.as_ref().expect("still choosing");
    let found: Vec<&str> = picker
        .matches()
        .into_iter()
        .map(|i| picker.options[i].value.as_str())
        .collect();
    assert_eq!(found, ["OpenAI"], "found by its description");
}

/// Esc in the add flow steps back one level at a time, then closes.
#[test]
fn esc_in_the_add_flow_steps_back_a_level() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);
    w.handle_key(press(KeyCode::Char('a')));
    w.handle_key(press(KeyCode::Enter));
    w.handle_key(press(KeyCode::Enter));
    assert_eq!(w.picker_purpose, keyed_text_providers(&w));

    w.handle_key(press(KeyCode::Esc));
    assert!(w.picker.is_some());
    assert_eq!(
        w.picker_purpose,
        PickerPurpose::Kind {
            category: "API key"
        }
    );

    w.handle_key(press(KeyCode::Esc));
    assert!(w.picker.is_some());
    assert_eq!(w.picker_purpose, PickerPurpose::Category);

    w.handle_key(press(KeyCode::Esc));
    assert!(w.picker.is_none(), "the top level closes");
    assert!(w.modal.is_none());
    assert_eq!(w.step, Step::Providers);
}

/// A provider already set up is still offered, marked, so choosing it again
/// reopens its modal rather than adding a second copy.
#[test]
fn a_configured_provider_is_marked_in_the_add_flow_and_reopens_its_modal() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.providers[0].value = "sk-ant".to_string();
    w.enter(Step::Providers);
    w.handle_key(press(KeyCode::Char('a')));
    w.handle_key(press(KeyCode::Enter));
    w.handle_key(press(KeyCode::Enter));

    let picker = w.picker.as_ref().expect("the provider chooser is open");
    let anthropic = picker
        .options
        .iter()
        .find(|o| o.value == "Anthropic")
        .expect("still offered");
    assert!(
        anthropic.detail.ends_with("(already set up)"),
        "{}",
        anthropic.detail
    );
    let openai = picker
        .options
        .iter()
        .find(|o| o.value == "OpenAI")
        .expect("offered");
    assert!(!openai.detail.ends_with("(already set up)"));

    w.handle_key(press(KeyCode::Enter));
    assert_eq!(w.modal_index(), Some(0));
    assert_eq!(
        w.providers[0].value, "sk-ant",
        "the modal opens on what is there"
    );
    assert_eq!(w.visible_providers(), vec![0], "no second copy");
}

/// The chooser is modal, so a letter is a letter: `q` looks for a provider
/// rather than quitting, and `a` does not restart the flow.
#[test]
fn letters_search_the_add_flow_rather_than_acting() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);
    w.handle_key(press(KeyCode::Char('a')));

    w.handle_key(press(KeyCode::Char('q')));
    assert!(!w.should_quit);
    w.handle_key(press(KeyCode::Char('a')));
    assert_eq!(w.picker.as_ref().expect("open").query.value(), "qa");
    assert_eq!(w.picker_purpose, PickerPurpose::Category);
}

// ─── the chooser ────────────────────────────────────────────────────────

/// A wizard on the Defaults screen with a model list worth searching.
fn wizard_with_models() -> (tempfile::TempDir, Wizard) {
    let (dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.providers[0].outcome = Outcome::Reachable {
        models: vec![
            "claude-opus-4".to_string(),
            "claude-sonnet-4-6".to_string(),
            "claude-haiku-4-5".to_string(),
        ],
    };
    w.show_advanced = true;
    w.enter(Step::Limits);
    w.cursor = Wizard::OVERRIDE_FIELD; // the override model choice
    (dir, w)
}

#[test]
fn enter_on_a_default_opens_a_chooser_that_says_what_it_decides() {
    let (_dir, mut w) = wizard_with_models();

    w.handle_key(press(KeyCode::Enter));

    let picker = w.picker.as_ref().expect("the chooser is open");
    assert_eq!(picker.title, "Override model");
    assert_eq!(
        w.picker_purpose,
        PickerPurpose::Field(Wizard::OVERRIDE_FIELD),
        "the chooser is for the field under the cursor"
    );
    assert!(
        !picker.explain.is_empty(),
        "the chooser exists to explain the value, not only to list it"
    );
    assert!(
        picker.options.iter().any(|o| o.value == "claude-opus-4"),
        "every discovered model is offered"
    );
    // Where a model came from is on the row, so a name nobody recognises is
    // still attributable.
    let opus = picker
        .options
        .iter()
        .find(|o| o.value == "claude-opus-4")
        .expect("listed");
    assert!(opus.detail.contains("Anthropic"), "{}", opus.detail);
}

#[test]
fn typing_filters_the_chooser_and_enter_takes_the_match() {
    let (_dir, mut w) = wizard_with_models();
    w.handle_key(press(KeyCode::Enter));

    type_str(&mut w, "haiku");
    let picker = w.picker.as_ref().expect("still open");
    assert_eq!(picker.matches().len(), 1, "one model matches 'haiku'");

    w.handle_key(press(KeyCode::Enter));

    assert!(w.picker.is_none(), "choosing closes it");
    assert_eq!(
        w.limits[Wizard::OVERRIDE_FIELD].value.display(),
        "claude-haiku-4-5"
    );
    assert!(w.dirty, "a chosen default is an unsaved change");
}

/// Every term has to land somewhere on the row, in any order, so a search
/// reads the way somebody would say the model out loud.
#[test]
fn the_search_matches_terms_in_any_order_and_across_the_detail() {
    let (_dir, mut w) = wizard_with_models();
    w.handle_key(press(KeyCode::Enter));

    type_str(&mut w, "4-6 claude");
    assert_eq!(
        w.picker.as_ref().expect("open").matches().len(),
        1,
        "terms are matched independently of their order"
    );

    for _ in 0.."4-6 claude".len() {
        w.handle_key(press(KeyCode::Backspace));
    }
    // The provider name is part of the row, so it is part of the search.
    type_str(&mut w, "anthropic");
    assert_eq!(w.picker.as_ref().expect("open").matches().len(), 3);

    // The "no default" row is the absence of a model, so it does not claim a
    // provider failed to report it.
    let none = &w.picker.as_ref().expect("open").options[0];
    assert_eq!(none.value, Wizard::NO_DEFAULT_MODEL);
    assert!(none.detail.starts_with("no default"), "{}", none.detail);
}

#[test]
fn escape_closes_the_chooser_and_keeps_the_value() {
    let (_dir, mut w) = wizard_with_models();
    let before = w.limits[Wizard::OVERRIDE_FIELD].value.display();
    w.handle_key(press(KeyCode::Enter));

    w.handle_key(press(KeyCode::Down));
    w.handle_key(press(KeyCode::Esc));

    assert!(w.picker.is_none());
    assert_eq!(w.limits[Wizard::OVERRIDE_FIELD].value.display(), before);
    assert!(!w.dirty, "looking is not changing");
}

/// A filter that matches nothing must not leave the cursor pointing past the
/// end of the list, and Enter on it must not choose whatever was there before.
#[test]
fn a_filter_that_matches_nothing_chooses_nothing() {
    let (_dir, mut w) = wizard_with_models();
    let before = w.limits[Wizard::OVERRIDE_FIELD].value.display();
    w.handle_key(press(KeyCode::Enter));

    type_str(&mut w, "zzzz");
    assert!(w.picker.as_ref().expect("open").selected().is_none());

    w.handle_key(press(KeyCode::Enter));
    assert!(w.picker.is_none(), "Enter still closes it");
    assert_eq!(w.limits[Wizard::OVERRIDE_FIELD].value.display(), before);
}

/// The chooser is modal, so a letter is a letter. `q` here means the user is
/// looking for Qwen.
#[test]
fn letters_search_rather_than_acting_while_the_chooser_is_open() {
    let (_dir, mut w) = wizard_with_models();
    w.handle_key(press(KeyCode::Enter));

    w.handle_key(press(KeyCode::Char('q')));

    assert!(!w.should_quit, "q must not quit out of a search box");
    assert_eq!(w.picker.as_ref().expect("open").query.value(), "q");
}

#[test]
fn the_priority_modal_lists_providers_with_their_names() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Defaults);
    w.cursor = 0;

    w.handle_key(press(KeyCode::Enter));

    let rows = w.reorder.as_ref().expect("open").rows_for_test();
    assert!(rows.iter().any(|(value, _)| value == "anthropic"));
    assert!(
        rows.iter().all(|(_, detail)| !detail.is_empty()),
        "an id alone does not say which service it is"
    );
}

/// Moving stops at the ends. Wrapping from the top of eighty models to the
/// bottom looks like the list jumped rather than moved.
#[test]
fn the_chooser_cursor_clamps_at_both_ends() {
    let (_dir, mut w) = wizard_with_models();
    w.handle_key(press(KeyCode::Enter));
    let total = w.picker.as_ref().expect("open").matches().len();

    w.handle_key(press(KeyCode::Home));
    assert_eq!(w.picker.as_ref().expect("open").cursor, 0);
    w.handle_key(press(KeyCode::Up));
    assert_eq!(w.picker.as_ref().expect("open").cursor, 0);

    w.handle_key(press(KeyCode::End));
    assert_eq!(w.picker.as_ref().expect("open").cursor, total - 1);
    w.handle_key(press(KeyCode::PageDown));
    assert_eq!(w.picker.as_ref().expect("open").cursor, total - 1);
    w.handle_key(press(KeyCode::PageUp));
    assert_eq!(w.picker.as_ref().expect("open").cursor, 0);
}

#[test]
fn clicking_a_row_in_the_chooser_takes_it_and_the_wheel_moves_within_it() {
    let (_dir, mut w) = wizard_with_models();
    w.handle_key(press(KeyCode::Enter));

    w.handle_mouse(wheel(true), AREA);
    let moved = w.picker.as_ref().expect("open").cursor;
    assert_eq!(
        moved, 1,
        "the wheel moves inside the chooser, not behind it"
    );

    // A click outside the list keeps the chooser open rather than discarding a
    // half-typed search.
    w.handle_mouse(click(4, 0), AREA);
    assert!(w.picker.is_some());

    let row = (0..AREA.height)
        .find(|y| w.picker.as_ref().expect("open").row_at(AREA, *y) == Some(2))
        .expect("the third match is on screen");
    w.handle_mouse(click(6, row), AREA);

    assert!(w.picker.is_none(), "a click on a row chooses it");
    // Row 0 is the "no default" option and the models sort after it.
    assert_eq!(
        w.limits[Wizard::OVERRIDE_FIELD].value.display(),
        "claude-opus-4"
    );
}

/// Page keys and Home/End are bound, not only reachable through the methods
/// the render tests call directly.
#[test]
fn the_page_and_edge_keys_move_the_selection() {
    let (_dir, mut w) = wizard();
    w.show_advanced = true;
    w.enter(Step::Limits);

    w.handle_key(press(KeyCode::PageDown));
    assert_eq!(w.cursor, Wizard::PAGE as usize);
    w.handle_key(press(KeyCode::PageUp));
    assert_eq!(w.cursor, 0);
    w.handle_key(press(KeyCode::End));
    assert_eq!(w.cursor, w.nav_rows() - 1);
    w.handle_key(press(KeyCode::Home));
    assert_eq!(w.cursor, 0);
}

/// Everything the terminal reports that is not a wheel or a left click is
/// ignored, on the screen and inside the chooser alike. Mouse movement in
/// particular arrives constantly once capture is on.
#[test]
fn mouse_events_that_are_not_a_click_or_a_wheel_are_ignored() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Providers);
    let moved = MouseEvent {
        kind: MouseEventKind::Moved,
        column: 4,
        row: 4,
        modifiers: KeyModifiers::empty(),
    };

    w.handle_mouse(moved, AREA);
    assert_eq!(w.cursor, 0);
    assert!(w.modal.is_none(), "hovering is not clicking");

    w.enter(Step::Defaults);
    w.open_picker(
        "Default provider",
        w.defaults[0].value.options().to_vec(),
        0,
    );
    w.handle_mouse(moved, AREA);
    assert!(w.picker.is_some(), "and it does not close the chooser");
    w.handle_mouse(wheel(false), AREA);
    assert_eq!(
        w.picker.as_ref().expect("open").cursor,
        0,
        "the wheel up stops at the top"
    );
}

/// A value the catalog has never heard of still reads as a choice: it is in
/// the config, so it is legitimate, and the row says where it came from.
#[test]
fn a_configured_provider_outside_the_catalog_still_describes_itself() {
    let dir = tempfile::tempdir().unwrap();
    let config = crate::config::Config {
        default_provider: "in-house".to_string(),
        override_model: Some("ghost-model".to_string()),
        ..Default::default()
    };
    let mut w = Wizard::new(
        config,
        &|_| None,
        Vec::new(),
        Vec::new(),
        dir.path(),
        std::sync::Arc::new(|_| true),
        Default::default(),
    );
    w.enter(Step::Defaults);
    w.cursor = 0;

    // The provider outside the catalog is still in the priority, described as
    // coming from the config file rather than dropped for being unknown.
    w.handle_key(press(KeyCode::Enter));
    let rows = w.reorder.take().expect("open").rows_for_test();
    assert_eq!(rows[0].0, "in-house");
    assert_eq!(rows[0].1, "from your config");

    w.show_advanced = true;
    w.enter(Step::Limits);
    w.cursor = Wizard::OVERRIDE_FIELD;
    w.open_picker(
        "Override model",
        w.limits[Wizard::OVERRIDE_FIELD].value.options().to_vec(),
        0,
    );
    let picker = w.picker.as_ref().expect("open");
    let ghost = picker
        .options
        .iter()
        .find(|o| o.value == "ghost-model")
        .expect("the configured model is offered even unreported");
    assert_eq!(ghost.detail, "not reported by a provider you selected");
}

/// The priority no longer takes every configured provider on its own: a
/// provider configured since is listed in the modal, left out, and Space
/// brings it in. Lifting it to the head then re-picks the concurrency
/// default, so a local-first setup does not keep a hosted-API number.
#[test]
fn bringing_a_provider_into_the_priority_and_lifting_it_repicks_the_concurrency_default() {
    let (_dir, mut w) = wizard();
    let ollama = provider(&w, "ollama");
    w.providers[0].selected = true;
    w.providers[ollama].selected = true;
    w.enter(Step::Defaults);
    w.cursor = 0;
    assert_eq!(
        w.defaults[0].value.display(),
        "anthropic",
        "only the default is seeded; ollama is not in the order yet"
    );

    w.handle_key(press(KeyCode::Enter));
    let reorder = w
        .reorder
        .as_ref()
        .expect("the priority field opens the reorder modal");
    let rows = reorder.rows_for_test();
    assert_eq!(rows[0].0, "anthropic");
    assert_eq!(rows[1].0, "ollama", "listed so it can be brought in");

    // Move onto ollama, bring it in, and lift it to the head.
    w.handle_key(press(KeyCode::Down));
    w.handle_key(press(KeyCode::Char(' ')));
    w.handle_key(press_with(KeyCode::Up, KeyModifiers::SHIFT));
    w.handle_key(press(KeyCode::Enter));

    assert!(
        w.reorder.is_none(),
        "Enter kept the order and closed the modal"
    );
    assert_eq!(w.defaults[0].value.display(), "ollama > anthropic");
    assert!(w.dirty);
    // The concurrency default follows the head provider, exactly as the picker
    // used to make it follow the single default.
    assert_eq!(
        w.limits[0].value.display(),
        crate::commands::setup::catalog::OLLAMA_MAX_CONCURRENT_INFERENCES.to_string()
    );
}

/// A row left out of the modal is not written: Enter keeps only the
/// included rows, and the last included one cannot be taken out.
#[test]
fn the_reorder_modal_writes_only_the_included_providers() {
    let (_dir, mut w) = wizard();
    let ollama = provider(&w, "ollama");
    w.providers[0].selected = true;
    w.providers[ollama].selected = true;
    w.enter(Step::Defaults);
    w.cursor = 0;

    // Enter with ollama still left out keeps the order as it was.
    w.handle_key(press(KeyCode::Enter));
    w.handle_key(press(KeyCode::Enter));
    assert!(w.reorder.is_none());
    assert_eq!(w.defaults[0].value.display(), "anthropic");

    // Space on the only included row is refused: an empty order is not one.
    w.handle_key(press(KeyCode::Enter));
    w.handle_key(press(KeyCode::Char(' ')));
    w.handle_key(press(KeyCode::Enter));
    assert_eq!(w.defaults[0].value.display(), "anthropic");

    // Bringing ollama in behind anthropic keeps the head, so the concurrency
    // default is untouched.
    let before = w.limits[0].value.display();
    w.handle_key(press(KeyCode::Enter));
    w.handle_key(press(KeyCode::Down));
    w.handle_key(press(KeyCode::Char(' ')));
    w.handle_key(press(KeyCode::Enter));
    assert_eq!(w.defaults[0].value.display(), "anthropic > ollama");
    assert_eq!(w.limits[0].value.display(), before);
}

/// Esc leaves the priority as it was, and the modal opens on the current
/// order rather than at a default.
#[test]
fn cancelling_the_reorder_keeps_the_priority() {
    let (_dir, mut w) = wizard();
    let ollama = provider(&w, "ollama");
    w.providers[0].selected = true;
    w.providers[ollama].selected = true;
    w.enter(Step::Defaults);
    w.cursor = 0;
    let before = w.defaults[0].value.display();
    w.handle_key(press(KeyCode::Enter));
    w.handle_key(press(KeyCode::Down));
    w.handle_key(press(KeyCode::Char(' ')));
    w.handle_key(press_with(KeyCode::Up, KeyModifiers::SHIFT));
    w.handle_key(press(KeyCode::Esc));
    assert!(w.reorder.is_none());
    assert_eq!(
        w.defaults[0].value.display(),
        before,
        "Esc discarded the move"
    );
    assert!(!w.dirty);
}

/// A mouse event reaches the open reorder modal through the loop's mouse
/// router, the same way it reaches an open chooser.
#[test]
fn a_mouse_event_reaches_the_open_reorder_modal() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Defaults);
    w.cursor = 0;
    w.handle_key(press(KeyCode::Enter));
    assert!(w.reorder.is_some());
    let area = ratatui::layout::Rect::new(0, 0, 90, 40);
    let scroll = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::ScrollDown,
        column: 1,
        row: 1,
        modifiers: KeyModifiers::empty(),
    };
    w.handle_mouse(scroll, area);
    assert!(
        w.reorder.is_some(),
        "a scroll moves the modal, it stays open"
    );
}

/// A reorder outcome is routed back into the field, so the mouse path is
/// wired the same as the keyboard one.
#[test]
fn a_confirmed_reorder_lands_in_the_priority_field() {
    let (_dir, mut w) = wizard();
    let ollama = provider(&w, "ollama");
    w.providers[0].selected = true;
    w.providers[ollama].selected = true;
    w.enter(Step::Defaults);
    w.cursor = 0;
    w.handle_key(press(KeyCode::Enter));
    // Bring ollama (row 1) in and move it to the head by keyboard, then
    // confirm - the mouse path is exercised in the reorder widget's own tests;
    // here we only need the wizard to route a reorder outcome back into the
    // field.
    w.handle_key(press(KeyCode::Down));
    w.handle_key(press(KeyCode::Char(' ')));
    w.handle_key(press_with(KeyCode::Up, KeyModifiers::SHIFT));
    w.handle_key(press(KeyCode::Enter));
    assert!(w.defaults[0].value.display().starts_with("ollama"));
}

/// Both TUIs answer the same key for help, so one habit works in both.
#[test]
fn f1_opens_the_wizard_help_too() {
    let (_dir, mut w) = wizard();
    w.handle_key(press(KeyCode::F(1)));
    assert!(w.show_help);

    // And the overlay scrolls rather than swallowing the key.
    w.handle_key(press(KeyCode::PageDown));
    assert!(w.show_help, "scrolling is not dismissing");
    assert!(w.help_scroll.get() > 0);

    w.handle_key(press(KeyCode::Esc));
    assert!(!w.show_help);
    assert_eq!(w.help_scroll.get(), 0, "closing resets it");

    // F1 and `?` reach the overlay from inside a provider's modal as well.
    w.enter(Step::Providers);
    w.open_provider_modal(0);
    w.handle_key(press(KeyCode::F(1)));
    assert!(w.show_help);
    w.handle_key(press(KeyCode::Esc));
    assert!(!w.show_help);
    assert_eq!(
        w.modal_index(),
        Some(0),
        "closing help does not cancel the modal"
    );
    w.handle_key(press(KeyCode::Char('?')));
    assert!(w.show_help);
}

// ─── OpenAI-compatible endpoints ────────────────────────────────────────

fn preset(w: &Wizard, id: &str) -> usize {
    w.providers
        .iter()
        .position(|r| r.provider.id == id)
        .expect("the preset is in the table")
}

/// Opening a preset's modal adds its first entry so the form has something
/// to show; Cancel takes it away again, and accepting keeps it.
#[test]
fn opening_a_presets_modal_adds_its_first_entry_and_cancel_takes_it_back() {
    let (_dir, mut w) = wizard();
    let lm_studio = preset(&w, "lm-studio");
    w.enter(Step::Providers);

    w.open_provider_modal(lm_studio);
    assert_eq!(w.endpoints.len(), 1);
    assert_eq!(w.endpoints[0].name, "lm-studio");
    assert_eq!(
        w.endpoints[0].base_url,
        crate::commands::setup::catalog::LM_STUDIO_URL
    );
    assert!(w.providers[lm_studio].selected);

    w.handle_key(press(KeyCode::Esc));
    assert!(w.modal.is_none());
    assert!(w.endpoints.is_empty(), "Cancel takes the entry away");
    assert!(!w.providers[lm_studio].selected);
    assert_eq!(w.message.as_deref(), Some("Cancelled; nothing changed."));

    // Opened again and accepted, the entry stays and the preset is listed.
    w.open_provider_modal(lm_studio);
    assert_eq!(w.endpoints.len(), 1, "one entry, not a second");
    press_skip_use(&mut w);
    assert!(w.modal.is_none());
    assert_eq!(w.endpoints.len(), 1);
    assert!(w.providers[lm_studio].selected);
    assert!(w.dirty);
    assert_eq!(w.message.as_deref(), Some("LM Studio is set up."));
    assert_eq!(w.visible_providers(), vec![lm_studio]);
}

/// Enter on each row of an entry's form does what the row says.
#[test]
fn enter_on_the_endpoint_form_edits_cycles_checks_removes_and_adds() {
    let (_dir, mut w) = wizard();
    let (mut requests, _replies) = w.take_verify_ends().expect("first take");
    let llama = preset(&w, "llama-cpp");
    w.enter(Step::Providers);
    w.open_provider_modal(llama);
    assert_eq!(w.detail_row(), Some(llama));
    assert_eq!(w.row_count(), 9);
    assert_eq!(w.nav_rows(), 12, "the form's rows plus the three buttons");

    // Name: the editor opens, typing lands, Enter commits.
    w.cursor = 0;
    w.handle_key(press(KeyCode::Enter));
    assert!(w.edit.is_some());
    type_str(&mut w, "-x");
    w.handle_key(press(KeyCode::Enter));
    assert_eq!(w.endpoints[0].name, "llama-cpp-x");

    // Default model: nothing to cycle yet, so a message and no change.
    w.cursor = 5;
    w.handle_key(press(KeyCode::Enter));
    assert_eq!(w.endpoints[0].default_model, None);
    assert!(w.message.take().is_some());
    w.endpoints[0].models = "a, b".to_string();
    w.handle_key(press(KeyCode::Enter));
    assert_eq!(w.endpoints[0].default_model.as_deref(), Some("a"));
    w.handle_key(press(KeyCode::Left));
    assert_eq!(w.endpoints[0].default_model.as_deref(), Some("b"));
    // Arrows elsewhere on the form do nothing.
    w.cursor = 1;
    w.handle_key(press(KeyCode::Right));
    assert_eq!(w.endpoints[0].default_model.as_deref(), Some("b"));

    // Check: a request goes out.
    w.cursor = 6;
    w.handle_key(press(KeyCode::Enter));
    assert!(w.endpoints[0].checking);
    assert_eq!(
        requests.try_recv().expect("sent").provider_id,
        "llama-cpp-x"
    );
    assert!(w.message.take().unwrap().contains("Checking"));
    assert!(
        w.modal.as_ref().is_some_and(|m| !m.awaiting_verify),
        "an entry's own check is not the Verify-and-use button"
    );

    // Add another: a second form appears; Remove takes it away and the
    // cursor stays inside the modal.
    w.cursor = 8;
    w.handle_key(press(KeyCode::Enter));
    assert_eq!(w.endpoints.len(), 2);
    assert_eq!(w.row_count(), 17);
    w.cursor = 8 + 7;
    w.handle_key(press(KeyCode::Enter));
    assert_eq!(w.endpoints.len(), 1);
    assert!(w.cursor <= w.row_count());
    assert_eq!(w.modal_index(), Some(llama));

    // "Skip verification and use" keeps the entry; the Continue button then
    // advances, and the entry's name is the default provider.
    press_skip_use(&mut w);
    assert!(w.modal.is_none());
    assert_eq!(w.message.as_deref(), Some("llama.cpp is set up."));
    assert_eq!(w.step, Step::Providers);
    w.cursor = w.row_count();
    w.handle_key(press(KeyCode::Enter));
    assert_eq!(w.step, Step::Defaults);
    assert_eq!(
        w.defaults[Wizard::PROVIDER_FIELD].value.display(),
        "llama-cpp-x"
    );
}

/// `v` in a preset's modal checks every entry under it and waits for all of
/// them: one failure keeps the modal open and names the entry, and a clean
/// round accepts.
#[test]
fn v_in_a_presets_modal_checks_every_entry_and_waits_for_them() {
    let (_dir, mut w) = wizard();
    let (mut requests, replies) = w.take_verify_ends().expect("first take");
    let custom = preset(&w, "openai-compatible");
    w.add_endpoint(custom);
    w.add_endpoint(custom);
    w.endpoints[0].base_url = "http://127.0.0.1:1/v1".to_string();
    w.endpoints[1].base_url = "http://127.0.0.1:2/v1".to_string();
    let names = [w.endpoints[0].name.clone(), w.endpoints[1].name.clone()];
    w.enter(Step::Providers);
    w.open_provider_modal(custom);
    assert_eq!(
        w.endpoints.len(),
        2,
        "entries already there are not added to"
    );

    w.handle_key(press(KeyCode::Char('v')));
    assert!(requests.try_recv().is_ok());
    assert!(requests.try_recv().is_ok());
    assert!(requests.try_recv().is_err());
    assert!(w.modal.as_ref().is_some_and(|m| m.awaiting_verify));
    assert_eq!(w.message.as_deref(), Some("Checking…"));

    // One answer in, one still out: nothing is decided yet.
    replies
        .send(VerifyReply {
            provider_id: names[0].clone(),
            outcome: Outcome::Failed {
                message: "refused".to_string(),
            },
        })
        .expect("held");
    w.drain_verifications();
    assert!(w.modal.as_ref().is_some_and(|m| m.awaiting_verify));

    replies
        .send(VerifyReply {
            provider_id: names[1].clone(),
            outcome: Outcome::Reachable { models: Vec::new() },
        })
        .expect("held");
    w.drain_verifications();
    assert_eq!(w.modal_index(), Some(custom), "a failure keeps it open");
    assert!(w.modal.as_ref().is_some_and(|m| !m.awaiting_verify));
    assert_eq!(
        w.message.as_deref(),
        Some(format!("Check failed for {}.", names[0]).as_str())
    );

    // Checked again with both passing, the modal accepts.
    w.handle_key(press(KeyCode::Char('v')));
    for name in &names {
        replies
            .send(VerifyReply {
                provider_id: name.clone(),
                outcome: Outcome::Reachable { models: Vec::new() },
            })
            .expect("held");
    }
    w.drain_verifications();
    assert!(w.modal.is_none());
    assert!(w.providers[custom].selected);
    assert_eq!(
        w.message.as_deref(),
        Some("Custom OpenAI-compatible endpoint is set up.")
    );
}

/// A preset whose entries were all removed inside the modal cannot be used:
/// there would be nothing to write.
#[test]
fn skip_verification_refuses_a_preset_with_no_entries() {
    let (_dir, mut w) = wizard();
    let lm_studio = preset(&w, "lm-studio");
    w.enter(Step::Providers);
    w.open_provider_modal(lm_studio);
    w.cursor = 7; // the entry's Remove button
    w.handle_key(press(KeyCode::Enter));
    assert!(w.endpoints.is_empty());
    assert_eq!(w.row_count(), 1, "the add row alone");

    press_skip_use(&mut w);

    assert_eq!(w.modal_index(), Some(lm_studio), "still open");
    assert_eq!(
        w.message.as_deref(),
        Some("Add an endpoint first, or cancel.")
    );
    assert!(!w.providers[lm_studio].selected);

    // The add row puts one back, and then it accepts.
    w.cursor = 0;
    w.handle_key(press(KeyCode::Enter));
    assert_eq!(w.endpoints.len(), 1);
    press_skip_use(&mut w);
    assert!(w.modal.is_none());
    assert!(w.providers[lm_studio].selected);
}

/// A cursor forced past the form's rows and the buttons (tests can do this;
/// keys cannot) acts on nothing.
#[test]
fn enter_past_the_endpoint_forms_rows_does_nothing() {
    let (_dir, mut w) = wizard();
    let llama = preset(&w, "llama-cpp");
    w.enter(Step::Providers);
    w.open_provider_modal(llama);
    w.cursor = w.nav_rows() + 5;
    w.handle_key(press(KeyCode::Enter));
    assert_eq!(w.modal_index(), Some(llama));
    assert_eq!(w.endpoints.len(), 1);
    assert!(w.edit.is_none());
}

/// The Escape that cancels an endpoint edit leaves the value alone, and the
/// modal open.
#[test]
fn cancelling_an_endpoint_edit_keeps_the_old_value() {
    let (_dir, mut w) = wizard();
    let llama = preset(&w, "llama-cpp");
    w.enter(Step::Providers);
    w.open_provider_modal(llama);
    w.cursor = 2;
    w.handle_key(press(KeyCode::Enter));
    w.handle_key(press(KeyCode::Char('k')));
    w.handle_key(press(KeyCode::Esc));
    assert!(w.endpoints[0].api_key.is_empty());
    assert!(w.edit.is_none());
    assert_eq!(w.modal_index(), Some(llama));
}
