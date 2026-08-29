use super::*;
use crate::commands::setup::state::{ConfirmPurpose, DetailAction, Edit, EditTarget, FieldValue};

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

    // The credential editor refuses an empty credential screen.
    w.enter(Step::ProviderDetail);
    w.open_credential_editor();
    assert!(w.edit.is_none());
}

#[test]
fn enter_on_a_forced_credential_row_with_no_provider_is_harmless() {
    let (_dir, mut w) = wizard();
    w.enter(Step::ProviderDetail);
    w.cursor = 1; // not the Continue button (row_count is 0 here)

    w.handle_key(press(KeyCode::Enter));

    assert!(w.edit.is_none());
    assert_eq!(w.step, Step::ProviderDetail);
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
    assert!(w.dirty, "a committed edit is an unsaved change");
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
    let ollama = w
        .providers
        .iter()
        .position(|r| r.provider.id == "ollama")
        .expect("ollama is offered");
    w.providers[ollama].selected = true;
    w.enter(Step::Defaults);
    w.cursor = 0; // the provider choice

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

#[test]
fn the_credential_screen_with_no_provider_continues_on_enter() {
    let (_dir, mut w) = wizard();
    w.enter(Step::ProviderDetail);

    w.handle_key(press(KeyCode::Enter));

    assert!(w.edit.is_none());
    assert_ne!(w.step, Step::ProviderDetail);
}

// ─── selection ──────────────────────────────────────────────────────────

#[test]
fn space_toggles_a_provider_and_resets_the_credential_walk() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);
    w.detail = 3;

    w.handle_key(press(KeyCode::Char(' ')));

    assert!(w.providers[0].selected);
    assert!(w.dirty, "a toggle is an unsaved change");
    assert_eq!(w.detail, 0, "the walk is relative to the new selection");

    w.handle_key(press(KeyCode::Char(' ')));
    assert!(!w.providers[0].selected);
}

#[test]
fn enter_on_a_provider_row_selects_it_rather_than_advancing() {
    // The reported trap: users pressed Enter expecting to select, and the
    // wizard advanced past the credentials screen instead.
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);

    w.handle_key(press(KeyCode::Enter));

    assert_eq!(w.step, Step::Providers, "Enter on a row must not advance");
    assert!(w.providers[0].selected, "Enter selects like Space");
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

// ─── the Continue button ────────────────────────────────────────────────

#[test]
fn enter_on_the_continue_button_advances() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Providers);
    w.cursor = w.row_count(); // the button
    assert!(w.on_continue());

    w.handle_key(press(KeyCode::Enter));

    assert_eq!(w.step, Step::ProviderDetail);
}

#[test]
fn the_cursor_walks_past_the_last_row_onto_the_button() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);
    let rows = w.row_count();
    for _ in 0..rows + 5 {
        w.handle_key(press(KeyCode::Down));
    }
    assert_eq!(w.cursor, rows, "clamped to the button, not past it");
    assert!(w.on_continue());
}

#[test]
fn continuing_with_no_providers_asks_first() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);
    w.cursor = w.row_count();

    w.handle_key(press(KeyCode::Enter));

    assert_eq!(w.step, Step::Providers, "blocked behind the dialog");
    let pending = w.confirm.as_ref().expect("the guard dialog is open");
    assert_eq!(pending.purpose, ConfirmPurpose::NoProviders);
    assert!(!pending.dialog.focus_yes, "Go back holds focus");

    // Enter on the default answer goes back to picking providers.
    w.handle_key(press(KeyCode::Enter));
    assert_eq!(w.step, Step::Providers);
    assert!(w.confirm.is_none());

    // Asking again and explicitly confirming continues anyway; with no
    // provider selected the credential screen self-skips, with a message.
    w.handle_key(press(KeyCode::Enter));
    w.handle_key(press(KeyCode::Char('y')));
    assert_eq!(w.step, Step::Defaults);
    assert_eq!(
        w.message.as_deref(),
        Some("Skipped Credentials: no selected provider needs setup.")
    );
}

#[test]
fn tab_from_providers_with_none_selected_hits_the_same_guard() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);

    w.handle_key(press(KeyCode::Tab));

    assert_eq!(w.step, Step::Providers);
    assert_eq!(
        w.confirm.as_ref().expect("dialog open").purpose,
        ConfirmPurpose::NoProviders
    );
}

#[test]
fn tab_with_a_provider_selected_advances_without_asking() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Providers);

    w.handle_key(press(KeyCode::Tab));

    assert!(w.confirm.is_none());
    assert_eq!(w.step, Step::ProviderDetail);
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
fn arrows_on_a_keyed_provider_do_not_change_anything() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::ProviderDetail);
    let before = w.providers[0].value.clone();

    w.handle_key(press(KeyCode::Right));
    w.handle_key(press(KeyCode::Left));

    assert_eq!(w.providers[0].value, before);
    assert!(w.edit.is_none());
    assert!(!w.dirty);
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
fn ctrl_r_reveals_even_while_typing_a_credential() {
    // Revealing is most useful mid-typo; the chord must not be eaten (or
    // inserted as a literal 'r') by the open editor.
    let (_dir, mut w) = wizard();
    w.edit = Some(credential_edit("sk-", true));

    w.handle_key(press_with(KeyCode::Char('r'), KeyModifiers::CONTROL));

    assert!(w.reveal);
    assert_eq!(w.edit.as_ref().expect("still editing").line.value(), "sk-");
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
        Default::default(),
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
        Default::default(),
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
        .position(|r| r.provider.signup_url.is_none())
        .expect("a provider with nowhere to go");
    w.enter(Step::Providers);
    w.cursor = index;

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

#[test]
fn enter_on_review_saves_immediately() {
    // No provider gates the save behind a dialog any more.
    let (_dir, mut w) = wizard();
    w.enter(Step::Review);

    let action = w.handle_key(press(KeyCode::Enter));
    assert_eq!(action, Action::Save);
    assert!(w.confirm.is_none());
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
fn clicking_a_provider_selects_and_toggles_it() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);
    let (x, y) = point_of_row(&w, 1);

    w.handle_mouse(click(x, y), AREA);

    assert_eq!(w.cursor, 1, "the click moves the selection to what it hit");
    assert!(w.providers[1].selected, "and acts on it, as Enter would");
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

/// The two actions that used to be shortcut keys and nothing else.
#[test]
fn the_credential_screen_offers_its_actions_as_clickable_rows() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::ProviderDetail);
    assert_eq!(
        w.detail_actions(),
        vec![DetailAction::OpenSignup, DetailAction::Verify],
        "a provider with a key page offers both"
    );

    let (x, y) = point_of_row(&w, 1);
    w.handle_mouse(click(x, y), AREA);
    assert!(
        w.message
            .as_deref()
            .is_some_and(|m| m.starts_with("Opened")),
        "clicking the signup row opens the page: {:?}",
        w.message
    );

    let (x, y) = point_of_row(&w, 2);
    w.handle_mouse(click(x, y), AREA);
    assert_eq!(w.message.as_deref(), Some("Checking…"));
}

/// A provider with nowhere to sign up offers only the check, and the rows stay
/// contiguous rather than leaving a gap where a button would have been. Every
/// keyed row in the catalog has a key page today, so the test strips one.
#[test]
fn a_provider_without_a_key_page_offers_only_the_check() {
    let (_dir, mut w) = wizard();
    w.providers[0].provider.signup_url = None;
    w.providers[0].selected = true;
    w.enter(Step::ProviderDetail);

    assert_eq!(w.detail_actions(), vec![DetailAction::Verify]);
    assert_eq!(w.row_count(), 2, "the credential row plus the one action");
}

#[test]
fn a_click_outside_the_body_does_nothing() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);
    let before = w.providers[0].selected;

    // The footer, which is not a row and must not be treated as the nearest one.
    w.handle_mouse(click(4, AREA.height - 1), AREA);

    assert_eq!(w.cursor, 0);
    assert_eq!(w.providers[0].selected, before);
}

#[test]
fn the_wheel_moves_the_selection_so_the_view_follows_it() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);

    w.handle_mouse(wheel(true), AREA);
    assert_eq!(w.cursor, 1);
    w.handle_mouse(wheel(false), AREA);
    assert_eq!(w.cursor, 0);
}

/// A click cannot mean anything while a dialog or an edit is up, and taking it
/// as a dismissal would throw away a half-typed credential.
#[test]
fn clicks_are_ignored_while_a_dialog_or_an_edit_is_open() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);
    w.edit = Some(credential_edit("half-typed", true));
    w.handle_mouse(click(4, 4), AREA);
    assert!(w.edit.is_some(), "the edit survives a stray click");
    assert!(!w.providers[0].selected);

    w.edit = None;
    w.open_quit_confirm();
    w.handle_mouse(click(4, 4), AREA);
    assert!(w.confirm.is_some());
    assert!(!w.providers[0].selected);
}

// ─── the chooser ────────────────────────────────────────────────────────

/// A wizard on the Defaults screen with a model list worth searching.
fn wizard_with_models() -> (tempfile::TempDir, Wizard) {
    let (dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.providers[0].outcome = crate::commands::setup::verify::Outcome::Reachable {
        models: vec![
            "claude-opus-4".to_string(),
            "claude-sonnet-4-6".to_string(),
            "claude-haiku-4-5".to_string(),
        ],
    };
    w.enter(Step::Defaults);
    w.cursor = 1; // the model choice
    (dir, w)
}

fn type_str(w: &mut Wizard, text: &str) {
    for c in text.chars() {
        w.handle_key(press(KeyCode::Char(c)));
    }
}

#[test]
fn enter_on_a_default_opens_a_chooser_that_says_what_it_decides() {
    let (_dir, mut w) = wizard_with_models();

    w.handle_key(press(KeyCode::Enter));

    let picker = w.picker.as_ref().expect("the chooser is open");
    assert_eq!(picker.title, "Default model");
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
    assert_eq!(w.defaults[1].value.display(), "claude-haiku-4-5");
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
    let before = w.defaults[1].value.display();
    w.handle_key(press(KeyCode::Enter));

    w.handle_key(press(KeyCode::Down));
    w.handle_key(press(KeyCode::Esc));

    assert!(w.picker.is_none());
    assert_eq!(w.defaults[1].value.display(), before);
    assert!(!w.dirty, "looking is not changing");
}

/// A filter that matches nothing must not leave the cursor pointing past the
/// end of the list, and Enter on it must not choose whatever was there before.
#[test]
fn a_filter_that_matches_nothing_chooses_nothing() {
    let (_dir, mut w) = wizard_with_models();
    let before = w.defaults[1].value.display();
    w.handle_key(press(KeyCode::Enter));

    type_str(&mut w, "zzzz");
    assert!(w.picker.as_ref().expect("open").selected().is_none());

    w.handle_key(press(KeyCode::Enter));
    assert!(w.picker.is_none(), "Enter still closes it");
    assert_eq!(w.defaults[1].value.display(), before);
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
fn the_provider_chooser_lists_providers_with_their_names() {
    let (_dir, mut w) = wizard();
    w.providers[0].selected = true;
    w.enter(Step::Defaults);
    w.cursor = 0;

    w.handle_key(press(KeyCode::Enter));

    let picker = w.picker.as_ref().expect("open");
    assert_eq!(picker.title, "Default provider");
    assert!(picker.options.iter().any(|o| o.value == "anthropic"));
    assert!(
        picker.options.iter().all(|o| !o.detail.is_empty()),
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
    assert_eq!(w.defaults[1].value.display(), "claude-opus-4");
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
    w.enter(Step::Providers);
    let moved = MouseEvent {
        kind: MouseEventKind::Moved,
        column: 4,
        row: 4,
        modifiers: KeyModifiers::empty(),
    };

    w.handle_mouse(moved, AREA);
    assert_eq!(w.cursor, 0);
    assert!(!w.providers[0].selected, "hovering is not clicking");

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
        default_model: Some("ghost-model".to_string()),
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

    w.open_picker(
        "Default provider",
        w.defaults[0].value.options().to_vec(),
        0,
    );
    let picker = w.picker.take().expect("open");
    assert_eq!(picker.options[0].value, "in-house");
    assert_eq!(picker.options[0].detail, "from your config");

    w.cursor = 1;
    w.open_picker("Default model", w.defaults[1].value.options().to_vec(), 0);
    let picker = w.picker.as_ref().expect("open");
    let ghost = picker
        .options
        .iter()
        .find(|o| o.value == "ghost-model")
        .expect("the configured model is offered even unreported");
    assert_eq!(ghost.detail, "not reported by a provider you selected");
}

/// Choosing a provider re-picks the concurrency default, the same way an arrow
/// press does, so a local-first setup does not keep a hosted-API number.
#[test]
fn choosing_a_provider_in_the_chooser_repicks_the_concurrency_default() {
    let (_dir, mut w) = wizard();
    let ollama = w
        .providers
        .iter()
        .position(|r| r.provider.id == "ollama")
        .expect("ollama is offered");
    w.providers[0].selected = true;
    w.providers[ollama].selected = true;
    w.enter(Step::Defaults);
    w.cursor = 0;
    w.handle_key(press(KeyCode::Enter));

    // Move onto ollama and take it.
    while w
        .picker
        .as_ref()
        .and_then(|p| p.selected())
        .map(|i| w.picker.as_ref().expect("open").options[i].value.clone())
        != Some("ollama".to_string())
    {
        w.handle_key(press(KeyCode::Down));
    }
    w.handle_key(press(KeyCode::Enter));

    assert_eq!(w.defaults[0].value.display(), "ollama");
    assert_eq!(
        w.limits[0].value.display(),
        crate::commands::setup::catalog::OLLAMA_MAX_CONCURRENT_INFERENCES.to_string()
    );
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
}

// ─── OpenAI-compatible endpoints ────────────────────────────────────────

fn preset(w: &Wizard, id: &str) -> usize {
    w.providers
        .iter()
        .position(|r| r.provider.id == id)
        .expect("the preset is in the table")
}

/// Space on a preset row adds the first entry; Space again drops every
/// entry under it.
#[test]
fn toggling_an_endpoint_preset_adds_and_removes_its_entries() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Providers);
    w.cursor = preset(&w, "lm-studio");

    w.handle_key(press(KeyCode::Char(' ')));
    assert_eq!(w.endpoints.len(), 1);
    assert_eq!(w.endpoints[0].name, "lm-studio");
    assert_eq!(
        w.endpoints[0].base_url,
        crate::commands::setup::catalog::LM_STUDIO_URL
    );
    assert!(w.providers[w.cursor].selected);

    w.handle_key(press(KeyCode::Char(' ')));
    assert!(w.endpoints.is_empty());
    assert!(!w.providers[w.cursor].selected);
    assert!(w.dirty);
}

/// Enter on each row of an entry's form does what the row says.
#[test]
fn enter_on_the_endpoint_form_edits_cycles_checks_removes_and_adds() {
    let (_dir, mut w) = wizard();
    let (mut requests, _replies) = w.take_verify_ends().expect("first take");
    let llama = preset(&w, "llama-cpp");
    w.add_endpoint(llama);
    w.enter(Step::ProviderDetail);
    assert_eq!(w.detail_row(), Some(llama));
    assert_eq!(w.row_count(), 9);

    // Name: the editor opens, typing lands, Enter commits.
    w.cursor = 0;
    w.handle_key(press(KeyCode::Enter));
    assert!(w.edit.is_some());
    for c in "-x".chars() {
        w.handle_key(press(KeyCode::Char(c)));
    }
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

    // Add another: a second form appears; Remove takes it away and the
    // cursor stays inside the screen.
    w.cursor = 8;
    w.handle_key(press(KeyCode::Enter));
    assert_eq!(w.endpoints.len(), 2);
    assert_eq!(w.row_count(), 17);
    w.cursor = 8 + 7;
    w.handle_key(press(KeyCode::Enter));
    assert_eq!(w.endpoints.len(), 1);
    assert!(w.cursor <= w.row_count());

    // The Continue button still advances.
    w.cursor = w.row_count();
    w.handle_key(press(KeyCode::Enter));
    assert_eq!(w.step, Step::Defaults);
    assert_eq!(
        w.defaults[Wizard::PROVIDER_FIELD].value.display(),
        "llama-cpp-x"
    );
}

/// `v` on the preset's screen checks every entry under it, and Tab does the
/// same on the way out.
#[test]
fn v_and_tab_check_every_entry_under_the_preset() {
    let (_dir, mut w) = wizard();
    let (mut requests, _replies) = w.take_verify_ends().expect("first take");
    let custom = preset(&w, "openai-compatible");
    w.add_endpoint(custom);
    w.add_endpoint(custom);
    w.endpoints[0].base_url = "http://127.0.0.1:1/v1".to_string();
    w.endpoints[1].base_url = "http://127.0.0.1:2/v1".to_string();
    w.enter(Step::ProviderDetail);

    w.handle_key(press(KeyCode::Char('v')));
    assert!(requests.try_recv().is_ok());
    assert!(requests.try_recv().is_ok());
    assert!(requests.try_recv().is_err());

    w.handle_key(press(KeyCode::Tab));
    assert!(requests.try_recv().is_ok());
    assert!(requests.try_recv().is_ok());
    assert_eq!(w.step, Step::Defaults);
}

/// A cursor forced past the form's rows (tests can do this; keys cannot)
/// acts on nothing.
#[test]
fn enter_past_the_endpoint_forms_rows_does_nothing() {
    let (_dir, mut w) = wizard();
    let llama = preset(&w, "llama-cpp");
    w.add_endpoint(llama);
    w.enter(Step::ProviderDetail);
    w.cursor = w.row_count() + 5;
    w.handle_key(press(KeyCode::Enter));
    assert_eq!(w.step, Step::ProviderDetail);
    assert_eq!(w.endpoints.len(), 1);
    assert!(w.edit.is_none());
}

/// The Escape that cancels an endpoint edit leaves the value alone.
#[test]
fn cancelling_an_endpoint_edit_keeps_the_old_value() {
    let (_dir, mut w) = wizard();
    let llama = preset(&w, "llama-cpp");
    w.add_endpoint(llama);
    w.enter(Step::ProviderDetail);
    w.cursor = 2;
    w.handle_key(press(KeyCode::Enter));
    w.handle_key(press(KeyCode::Char('k')));
    w.handle_key(press(KeyCode::Esc));
    assert!(w.endpoints[0].api_key.is_empty());
    assert!(w.edit.is_none());
}
