use super::*;
use crate::commands::setup::state::{ConfirmPurpose, Edit, EditTarget, FieldValue};

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
    assert!(!w.open_credential_editor());
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
fn the_claude_code_transport_has_nothing_to_type_so_enter_cycles_effort() {
    let (_dir, mut w) = wizard();
    let index = w
        .providers
        .iter()
        .position(|r| r.provider.id == "claude-code")
        .expect("the transport is offered");
    w.providers[index].selected = true;
    w.enter(Step::ProviderDetail);
    let before = w.providers[index].effort;

    w.handle_key(press(KeyCode::Enter));

    assert!(w.edit.is_none());
    assert_eq!(
        w.step,
        Step::ProviderDetail,
        "acting on a row never advances"
    );
    assert_ne!(w.providers[index].effort, before, "Enter acts on the row");
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
fn enter_on_a_choice_cycles_it_and_stays() {
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

    w.handle_key(press(KeyCode::Enter));

    assert_eq!(w.step, Step::Defaults);
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

// ─── Claude Code ToS confirmation gate ──────────────────────────────────

fn wizard_with_claude_code() -> (tempfile::TempDir, Wizard) {
    let (dir, mut w) = wizard();
    let index = w
        .providers
        .iter()
        .position(|r| r.provider.id == "claude-code")
        .expect("the transport is offered");
    w.providers[index].selected = true;
    (dir, w)
}

#[test]
fn enter_on_review_with_claude_code_shows_tos_confirmation() {
    let (_dir, mut w) = wizard_with_claude_code();
    w.enter(Step::Review);

    let action = w.handle_key(press(KeyCode::Enter));

    assert_eq!(
        action,
        Action::Continue,
        "must not save without ToS acceptance"
    );
    assert_eq!(
        w.confirm.as_ref().expect("dialog open").purpose,
        ConfirmPurpose::SaveTos
    );
}

#[test]
fn ctrl_s_with_claude_code_shows_tos_confirmation() {
    let (_dir, mut w) = wizard_with_claude_code();
    w.enter(Step::Providers);

    let action = w.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));

    assert_eq!(action, Action::Continue);
    assert_eq!(
        w.confirm.as_ref().expect("dialog open").purpose,
        ConfirmPurpose::SaveTos
    );
}

#[test]
fn pressing_y_on_tos_confirmation_accepts_and_saves() {
    let (_dir, mut w) = wizard_with_claude_code();
    w.enter(Step::Review);
    w.handle_key(press(KeyCode::Enter));
    assert!(w.confirm.is_some());

    let action = w.handle_key(press(KeyCode::Char('y')));

    assert!(w.claude_code_tos_accepted);
    assert!(w.confirm.is_none());
    assert_eq!(
        action,
        Action::Save,
        "confirming is the last word on the save the user already asked for"
    );
}

#[test]
fn moving_focus_and_pressing_enter_also_accepts() {
    let (_dir, mut w) = wizard_with_claude_code();
    w.enter(Step::Review);
    w.handle_key(press(KeyCode::Enter));

    w.handle_key(press(KeyCode::Right)); // focus Accept
    let action = w.handle_key(press(KeyCode::Enter));

    assert!(w.claude_code_tos_accepted);
    assert_eq!(action, Action::Save);
}

#[test]
fn dismissing_the_tos_confirmation_stays_put_without_accepting() {
    let (_dir, mut w) = wizard_with_claude_code();
    w.enter(Step::Review);
    w.handle_key(press(KeyCode::Enter));

    let action = w.handle_key(press(KeyCode::Char('n')));

    assert!(!w.claude_code_tos_accepted);
    assert!(w.confirm.is_none());
    assert_eq!(action, Action::Continue, "the save stays blocked");
    assert_eq!(w.step, Step::Review, "declining must not navigate away");
}

#[test]
fn second_enter_on_review_after_accepting_saves() {
    let (_dir, mut w) = wizard_with_claude_code();
    w.enter(Step::Review);
    w.handle_key(press(KeyCode::Enter)); // shows the dialog
    w.handle_key(press(KeyCode::Char('y'))); // accept

    w.enter(Step::Review);
    let action = w.handle_key(press(KeyCode::Enter));

    assert_eq!(action, Action::Save, "should save after ToS accepted");
}

#[test]
fn deselecting_claude_code_resets_tos_acceptance() {
    let (_dir, mut w) = wizard_with_claude_code();
    w.claude_code_tos_accepted = true;

    let index = w
        .providers
        .iter()
        .position(|r| r.provider.id == "claude-code")
        .unwrap();
    w.enter(Step::Providers);
    w.cursor = index;
    w.handle_key(press(KeyCode::Char(' '))); // deselect

    assert!(!w.claude_code_tos_accepted);
}

#[test]
fn a_dialog_holds_focus_against_stray_keys() {
    let (_dir, mut w) = wizard_with_claude_code();
    w.enter(Step::Review);
    w.handle_key(press(KeyCode::Enter));
    assert!(w.confirm.is_some());

    // q neither quits nor dismisses: a stray key never answers a dialog.
    w.handle_key(press(KeyCode::Char('q')));
    assert!(!w.should_quit, "q must not quit while a dialog is open");
    assert!(w.confirm.is_some(), "and must not dismiss it either");

    // Esc explicitly declines.
    w.handle_key(press(KeyCode::Esc));
    assert!(w.confirm.is_none());
    assert!(!w.claude_code_tos_accepted);
}

#[test]
fn without_claude_code_review_saves_immediately() {
    let (_dir, mut w) = wizard();
    w.enter(Step::Review);

    let action = w.handle_key(press(KeyCode::Enter));
    assert_eq!(action, Action::Save, "no claude-code means no gate");
}
