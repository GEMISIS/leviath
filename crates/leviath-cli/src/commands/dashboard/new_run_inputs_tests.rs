//! The new-run screen's Inputs pane: its rows, their keys, how they are
//! read and checked, and a refused run brought back to them.

use super::*;
use crate::commands::dashboard::test_support::make_test_dashboard;
use crate::commands::dashboard::types::{NewRunContext, RefusedRun, SpawnOutcome};
use crossterm::event::KeyModifiers;
use leviath_runtime::spec::inputs::InputValue;
use leviath_runtime::spec::names::{ChoiceName, InputName, MimePattern};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// Text, as the wire carries it.
fn text(s: &str) -> RawInput {
    RawInput::Text(s.to_string())
}

/// Plain text, unbounded.
fn text_type() -> InputType {
    InputType::Text {
        multiline: false,
        min_len: None,
        max_len: None,
    }
}

/// An input named `name` of type `ty`.
fn decl(name: &str, ty: InputType, required: bool) -> InputDecl {
    InputDecl {
        name: InputName::new(name).unwrap(),
        ty,
        required,
        default: None,
        description: None,
        binds: Vec::new(),
    }
}

/// An agent with two caller inputs beside its task: a typed picture slot
/// and a plain notes slot.
fn write_agent(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("agent.toml"),
        r#"[blueprint]
name = "looker"
version = "0.1.0"
description = "looks"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }

[graph.layout]
total_budget_tokens = 112000

[[graph.layout.regions]]
name = "task"
kind = "pinned"
budget = 1000

[[graph.layout.regions]]
name = "pictures"
kind = "pinned"
budget = 100000
required = true
accepts = ["image/*"]

[[graph.layout.regions]]
name = "notes"
kind = "pinned"
budget = 1000

[[graph.layout.regions]]
name = "conversation"
kind = { kind = "sliding_window", max_items = 20 }
budget = 10000

[[graph.inputs]]
name = "notes"
type = { kind = "text", multiline = true }
binds = [{ region = "notes" }]

[[graph.inputs]]
name = "pictures"
type = { kind = "text", multiline = true }
required = true
binds = [{ region = "pictures" }]

[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
binds = [{ region = "task" }]
"#,
    )
    .unwrap();
}

fn dash_at(dir: &Path) -> Dashboard {
    let mut dash = make_test_dashboard();
    dash.new_run_ctx = NewRunContext {
        agents_dir: dir.join("agents"),
        config_path: dir.join("config.toml"),
        workdir: dir.join("work"),
    };
    std::fs::create_dir_all(dir.join("work")).unwrap();
    // The catalog lists the bundled blueprints too, ahead of `looker`
    // alphabetically; the screen opens on the agent last launched.
    dash.last_launched_agent = Some("looker".to_string());
    dash
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn type_str(dash: &mut Dashboard, s: &str) {
    for c in s.chars() {
        dash.handle_new_run_key(key(KeyCode::Char(c)));
    }
}

fn screen(dash: &mut Dashboard) -> String {
    let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
    terminal.draw(|f| dash.draw(f)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect()
}

/// The rows follow the selected agent: one per input, in the blueprint's
/// order, with the task left to the task box.
#[test]
fn the_slots_are_the_blueprints_caller_inputs() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(&dir.path().join("agents").join("looker"));
    let mut dash = dash_at(dir.path());
    dash.open_new_run_screen();
    let keys: Vec<&str> = dash.new_run_inputs.iter().map(|s| s.key.as_str()).collect();
    assert_eq!(keys, ["pictures", "notes"]);
    assert_eq!(dash.new_run_inputs[0].region, "pictures");
    assert_eq!(dash.new_run_inputs[0].accepts, ["image/*"]);
    assert!(dash.new_run_inputs[0].required);
    // The note names the type, the token budget, and that it is required.
    assert_eq!(
        dash.new_run_inputs[0].note(),
        " (image/*, ≤100k tok, required)"
    );
    assert_eq!(dash.new_run_inputs[1].note(), " (≤1k tok)");
    assert!(dash.new_run_has_inputs());
    assert_eq!(dash.new_run_inputs_height(), 4);
    // The same agent again keeps the slots; no agent clears them.
    dash.sync_new_run_inputs();
    assert_eq!(dash.new_run_inputs.len(), 2);
    dash.new_run_agents.clear();
    dash.sync_new_run_inputs();
    assert!(!dash.new_run_has_inputs());
    assert_eq!(dash.new_run_inputs_height(), 0);
}

/// Tab walks agents → inputs → task → start and back; Enter in the last
/// slot moves on to the task; Esc goes back to the agents.
#[test]
fn the_keys_walk_the_slots_and_the_panes() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(&dir.path().join("agents").join("looker"));
    let mut dash = dash_at(dir.path());
    dash.open_new_run_screen();
    dash.handle_new_run_key(key(KeyCode::Tab));
    assert_eq!(dash.new_run_focus, NewRunPane::Inputs);
    dash.handle_new_run_key(key(KeyCode::Down));
    assert_eq!(dash.new_run_input_selected, 1);
    dash.handle_new_run_key(key(KeyCode::Down));
    assert_eq!(dash.new_run_input_selected, 1, "stops at the last");
    dash.handle_new_run_key(key(KeyCode::Up));
    assert_eq!(dash.new_run_input_selected, 0);
    // Row 0 (pictures) takes images only: Enter opens the picker, not text.
    assert_eq!(dash.new_run_input_selected, 0);
    dash.handle_new_run_key(key(KeyCode::Enter));
    assert!(dash.new_run_picker_open(), "a file row opens the picker");
    dash.handle_new_run_key(key(KeyCode::Esc));
    assert!(!dash.new_run_picker_open());
    // Row 1 (notes) takes text: typing lands there and Enter on the last
    // slot moves on to the task.
    dash.new_run_input_selected = 1;
    type_str(&mut dash, "be brief");
    assert_eq!(dash.new_run_inputs[1].edit.value(), "be brief");
    dash.handle_new_run_key(key(KeyCode::Enter));
    assert_eq!(dash.new_run_focus, NewRunPane::Task, "and on from the last");
    dash.handle_new_run_key(key(KeyCode::BackTab));
    assert_eq!(dash.new_run_focus, NewRunPane::Inputs);
    dash.handle_new_run_key(key(KeyCode::Tab));
    assert_eq!(dash.new_run_focus, NewRunPane::Task);
    dash.new_run_focus = NewRunPane::Inputs;
    dash.handle_new_run_key(key(KeyCode::Esc));
    assert_eq!(dash.new_run_focus, NewRunPane::Agents);
    dash.new_run_focus = NewRunPane::Inputs;
    dash.handle_new_run_key(key(KeyCode::BackTab));
    assert_eq!(dash.new_run_focus, NewRunPane::Agents);
    // A slot's own Esc is the pane's Esc, never a cancel that eats text.
    dash.new_run_focus = NewRunPane::Inputs;
    dash.new_run_input_selected = 1;
    assert_eq!(dash.new_run_inputs[1].edit.value(), "be brief");
    // With no slots the pane is skipped both ways.
    dash.new_run_agents.clear();
    dash.sync_new_run_inputs();
    dash.new_run_focus = NewRunPane::Agents;
    dash.handle_new_run_key(key(KeyCode::Tab));
    assert_eq!(dash.new_run_focus, NewRunPane::Task);
    dash.handle_new_run_key(key(KeyCode::BackTab));
    assert_eq!(dash.new_run_focus, NewRunPane::Agents);
    // Keys on an empty Inputs pane do nothing.
    dash.new_run_focus = NewRunPane::Inputs;
    dash.handle_new_run_key(key(KeyCode::Char('x')));
    assert!(dash.new_run_inputs.is_empty());
}

/// A bare file name attaches the file to its region, `@file` does too,
/// text seeds the region under its caller key, a text file seeds it with
/// the file's text, and a path that names nothing is an error naming the
/// slot.
#[test]
fn the_slots_resolve_the_way_the_region_flags_do() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(&dir.path().join("agents").join("looker"));
    let mut dash = dash_at(dir.path());
    let work = dir.path().join("work");
    std::fs::write(work.join("hero.png"), b"\x89PNG\r\n\x1a\nbody").unwrap();
    std::fs::write(work.join("notes.md"), "read me").unwrap();
    dash.open_new_run_screen();
    // Nothing typed: nothing sent.
    let none = dash.new_run_input_values().unwrap();
    assert!(none.values.is_empty() && none.parts.is_empty());

    dash.new_run_inputs[0].edit = LineEdit::new("hero.png", false);
    dash.new_run_inputs[1].edit = LineEdit::new("look at @hero.png and @ghost.png", false);
    let got = dash.new_run_input_values().unwrap();
    assert_eq!(got.parts.len(), 2);
    assert_eq!(got.parts[0].region.as_deref(), Some("pictures"));
    assert_eq!(got.parts[0].name, "hero.png");
    assert_eq!(got.parts[1].region.as_deref(), Some("notes"));
    assert_eq!(
        got.values.get("notes"),
        Some(&text("look at @hero.png and @ghost.png"))
    );
    assert_eq!(got.unresolved, ["ghost.png"]);

    dash.new_run_inputs[0].edit = LineEdit::new("@hero.png", false);
    dash.new_run_inputs[1].edit = LineEdit::new("@notes.md", false);
    let got = dash.new_run_input_values().unwrap();
    assert_eq!(got.parts.len(), 1);
    assert_eq!(got.values.get("notes"), Some(&text("read me")));

    dash.new_run_inputs[0].edit = LineEdit::new("@missing.png", false);
    let err = dash.new_run_input_values().unwrap_err();
    assert!(err.starts_with("pictures:"), "{err}");
}

/// Starting the run sends the slots: the picture as a part in its region,
/// the notes as a seed, beside whatever the task named.
#[test]
fn the_run_carries_the_slots() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(&dir.path().join("agents").join("looker"));
    let mut dash = dash_at(dir.path());
    let work = dir.path().join("work");
    std::fs::write(work.join("hero.png"), b"\x89PNG\r\n\x1a\nbody").unwrap();
    dash.open_new_run_screen();
    dash.new_run_inputs[0].edit = LineEdit::new("hero.png", false);
    dash.new_run_inputs[1].edit = LineEdit::new("be brief", false);
    dash.new_run_task.area_mut().insert_str("describe it");
    dash.submit_new_run();
    let cmd = dash.spawn_cmd_rx_for_test().try_recv().unwrap();
    assert_eq!(cmd.parts.len(), 1);
    assert_eq!(cmd.parts[0].region.as_deref(), Some("pictures"));
    assert_eq!(cmd.values.get("notes"), Some(&text("be brief")));
    // A slot that cannot be read stops the start with a toast naming it.
    let mut dash = dash_at(dir.path());
    dash.open_new_run_screen();
    dash.new_run_inputs[0].edit = LineEdit::new("@nope.png", false);
    dash.new_run_task.area_mut().insert_str("describe it");
    dash.submit_new_run();
    assert!(dash.spawn_cmd_rx_for_test().try_recv().is_err());
    let toasts = dash.toast_messages_for_test();
    assert!(toasts.iter().any(|t| t.contains("pictures:")), "{toasts:?}");
}

/// The pane draws between the preview and the task with a row per slot,
/// says what each takes, and a click on a row picks it.
#[test]
fn the_pane_draws_its_rows_and_takes_a_click() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(&dir.path().join("agents").join("looker"));
    let mut dash = dash_at(dir.path());
    dash.open_new_run_screen();
    let text = screen(&mut dash);
    assert!(text.contains("Inputs for looker"), "{text}");
    assert!(
        text.contains("pictures (image/*, ≤100k tok, required)"),
        "{text}"
    );
    // The pictures row takes images only, so it prompts for a file rather
    // than a line of text.
    assert!(text.contains("no files chosen"), "{text}");
    let rect = dash
        .click_targets
        .iter()
        .find(|(_, t)| *t == ClickTarget::NewRunInput(1))
        .map(|(r, _)| *r)
        .expect("the second slot is clickable");
    assert!(dash.handle_click(rect.x + 2, rect.y));
    assert_eq!(dash.new_run_focus, NewRunPane::Inputs);
    assert_eq!(dash.new_run_input_selected, 1);
    dash.click_new_run_input(9);
    assert_eq!(
        dash.new_run_input_selected, 1,
        "a row that is not there is ignored"
    );
    // A focused slot shows its text where the placeholder was.
    dash.new_run_inputs[1].edit = LineEdit::new("be brief", false);
    let text = screen(&mut dash);
    assert!(text.contains("be brief"), "{text}");
}

/// A slot with nothing worth noting (no type, one file, no budget, not
/// required) shows no note at all.
#[test]
fn a_plain_slot_has_no_note() {
    let slot = NewRunInput::new(&decl("x", text_type(), false));
    assert_eq!(slot.note(), "");
}

/// A file slot shows the files it holds: the names, the count against the
/// cap for a many-file slot, and a chip of extra files beside typed text on
/// a slot that takes both.
#[test]
fn a_file_row_shows_its_chosen_files() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(&dir.path().join("agents").join("looker"));
    let mut dash = dash_at(dir.path());
    dash.open_new_run_screen();
    // Focused and empty, the file slot prompts to choose.
    dash.new_run_focus = NewRunPane::Inputs;
    dash.new_run_input_selected = 0;
    let text = screen(&mut dash);
    assert!(text.contains("Enter to choose files"), "{text}");
    // A chosen path with no final component still renders its name.
    dash.new_run_inputs[0].files = vec![PathBuf::from("..")];
    let text = screen(&mut dash);
    assert!(text.contains("pictures"), "{text}");
    // The pictures slot (image/*) holds one file: the name shows, no count.
    dash.new_run_inputs[0].files = vec![PathBuf::from("out/hero.png")];
    let text = screen(&mut dash);
    assert!(text.contains("hero.png"), "{text}");
    // Several files show a count.
    dash.new_run_inputs[0].files = vec![PathBuf::from("a.png"), PathBuf::from("b.png")];
    let text = screen(&mut dash);
    assert!(text.contains("(2)"), "{text}");
    // The notes slot takes anything, so text and a file chip sit together.
    dash.new_run_inputs[1].edit = LineEdit::new("look", false);
    dash.new_run_inputs[1].files = vec![PathBuf::from("c.png")];
    let text = screen(&mut dash);
    assert!(text.contains("look"), "{text}");
    assert!(text.contains("+c.png"), "{text}");
}

/// An empty text slot keeps its hint while it has focus, so it never looks
/// blank and nobody forgets what it wants.
#[test]
fn an_empty_text_slot_keeps_its_hint_while_focused() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(&dir.path().join("agents").join("looker"));
    let mut dash = dash_at(dir.path());
    dash.open_new_run_screen();
    // Focus the notes slot (takes text and files); empty, it still shows
    // the hint.
    dash.new_run_focus = NewRunPane::Inputs;
    dash.new_run_input_selected = 1;
    let text = screen(&mut dash);
    assert!(text.contains("text, or ^O for files"), "{text}");
}

/// A file slot opens the picker by key: Space (or Enter) on a file-only
/// slot, and Ctrl+O on one that also takes text, which still types
/// otherwise.
#[test]
fn a_file_row_opens_the_picker_by_key() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(&dir.path().join("agents").join("looker"));
    let mut dash = dash_at(dir.path());
    dash.open_new_run_screen();
    dash.new_run_focus = NewRunPane::Inputs;
    // Space on the file-only pictures slot opens the picker.
    dash.new_run_input_selected = 0;
    dash.handle_new_run_key(key(KeyCode::Char(' ')));
    assert!(dash.new_run_picker_open());
    dash.handle_new_run_key(key(KeyCode::Esc));
    // Ctrl+O on the notes slot (which takes anything) opens it too.
    dash.new_run_input_selected = 1;
    dash.handle_new_run_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert!(dash.new_run_picker_open());
    dash.handle_new_run_key(key(KeyCode::Esc));
    // A plain letter on that slot still types.
    dash.handle_new_run_key(key(KeyCode::Char('h')));
    assert!(!dash.new_run_picker_open());
    assert_eq!(dash.new_run_inputs[1].edit.value(), "h");
    // On the file-only slot, a control chord that is not Ctrl+O, and a
    // plain letter, both do nothing: no picker, no text.
    dash.new_run_input_selected = 0;
    dash.handle_new_run_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
    assert!(!dash.new_run_picker_open());
    dash.handle_new_run_key(key(KeyCode::Char('z')));
    assert!(!dash.new_run_picker_open());
    assert!(dash.new_run_inputs[0].edit.value().is_empty());
}

/// Enter on a text slot that is not the last moves to the next slot.
#[test]
fn enter_advances_between_text_slots() {
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agents").join("noter");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("agent.toml"),
        r#"[blueprint]
name = "noter"
version = "0.1.0"
description = "notes"

[[graph.stages]]
name = "main"
model = { models = [{ provider = "anthropic", model = "claude-sonnet-5" }] }

[graph.layout]
regions = [
    { name = "task", kind = "pinned", budget = 1000 },
    { name = "one", kind = "pinned", budget = 1000, accepts = ["text/*"] },
    { name = "two", kind = "pinned", budget = 1000, accepts = ["text/*"] },
]
total_budget_tokens = 3000

[[graph.inputs]]
name = "one"
type = { kind = "text", multiline = true }
binds = [{ region = "one" }]

[[graph.inputs]]
name = "task"
type = { kind = "text", multiline = true }
binds = [{ region = "task" }]

[[graph.inputs]]
name = "two"
type = { kind = "text", multiline = true }
binds = [{ region = "two" }]
"#,
    )
    .unwrap();
    let mut dash = make_test_dashboard();
    dash.new_run_ctx = NewRunContext {
        agents_dir: dir.path().join("agents"),
        config_path: dir.path().join("config.toml"),
        workdir: dir.path().join("work"),
    };
    std::fs::create_dir_all(dir.path().join("work")).unwrap();
    dash.last_launched_agent = Some("noter".to_string());
    dash.open_new_run_screen();
    assert_eq!(dash.new_run_inputs.len(), 2);
    dash.new_run_focus = NewRunPane::Inputs;
    dash.new_run_input_selected = 0;
    dash.handle_new_run_key(key(KeyCode::Char('a')));
    dash.handle_new_run_key(key(KeyCode::Enter));
    assert_eq!(
        dash.new_run_input_selected, 1,
        "Enter on a non-last text slot advances"
    );
}

/// The entry model's window resolves offline: from the compiled catalog,
/// from a `[model_capabilities]` override, and from the default when the
/// model is unknown or the stage names none.
#[test]
fn the_entry_window_resolves() {
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agents").join("looker");
    write_agent(&agent);
    let agent_path = agent.to_str().unwrap();
    let missing = dir.path().join("no-config.toml");

    // A known model uses the compiled catalog window.
    let builtin = crate::commands::models::builtin_model_windows()
        .get(&("anthropic".to_string(), "claude-sonnet-5".to_string()))
        .copied()
        .expect("claude-sonnet-5 is in the catalog");
    let bp = super::super::graph::load_blueprint(agent_path).unwrap();
    assert_eq!(entry_stage_window(&bp, &missing, None), builtin);

    // A stage that names no model falls back to the default window.
    let mut bp = super::super::graph::load_blueprint(agent_path).unwrap();
    bp.stages[0].model.models.clear();
    assert_eq!(entry_stage_window(&bp, &missing, None), 8192);

    // A `[model_capabilities]` override for this model wins.
    let bp = super::super::graph::load_blueprint(agent_path).unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "[model_capabilities.\"anthropic/claude-sonnet-5\"]\nmax_context_tokens = 4321\n",
    )
    .unwrap();
    assert_eq!(entry_stage_window(&bp, &config, None), 4321);

    // An unknown model, checked against a config that loads but has no entry
    // for it, falls past the override lookup to the catalog and then to the
    // default.
    let mut bp = super::super::graph::load_blueprint(agent_path).unwrap();
    bp.stages[0].model.models[0] = model_ref("acme/mystery");
    assert_eq!(entry_stage_window(&bp, &config, None), 8192);

    // A config file that cannot be parsed is ignored, and the window comes
    // from the catalog.
    let bp = super::super::graph::load_blueprint(agent_path).unwrap();
    let bad = dir.path().join("bad.toml");
    std::fs::write(&bad, "this is not [valid toml").unwrap();
    assert_eq!(entry_stage_window(&bp, &bad, None), builtin);

    // The shared capability cache supplies a window for a model absent from
    // the compiled table - the offline OpenRouter case #810 is about.
    let mut bp = super::super::graph::load_blueprint(agent_path).unwrap();
    bp.stages[0].model.models[0] = model_ref("openrouter/x-ai/grok-4");
    let cache_file = dir.path().join("model_capabilities.json");
    let cache = {
        let mut c = leviath_providers::CapabilityCache::new(1);
        c.set(
            "openrouter",
            std::collections::BTreeMap::from([(
                "x-ai/grok-4".to_string(),
                leviath_providers::LearnedModel {
                    max_context_tokens: Some(256_000),
                    ..Default::default()
                },
            )]),
        );
        c
    };
    cache.save(&cache_file).unwrap();
    assert_eq!(
        entry_stage_window(&bp, &missing, Some(&cache_file)),
        256_000
    );
    // A cache path that does not exist loads nothing and falls to the
    // default.
    let no_cache = dir.path().join("absent.json");
    assert_eq!(entry_stage_window(&bp, &missing, Some(&no_cache)), 8192);

    // The smallest window across several models wins: a second, narrower
    // model pulls the effective window down.
    let mut bp = super::super::graph::load_blueprint(agent_path).unwrap();
    bp.stages[0].model.models[0] = model_ref("anthropic/claude-sonnet-5");
    bp.stages[0].model.models.push(model_ref("acme/tiny"));
    // acme/tiny is unknown everywhere → 8192, smaller than the sonnet
    // window, so it is the effective window.
    assert_eq!(entry_stage_window(&bp, &missing, None), 8192);

    // A model that leaves its provider open is looked up under its bare name:
    // a `[model_capabilities]` override for that name answers, and with none
    // the default does.
    let mut bp = super::super::graph::load_blueprint(agent_path).unwrap();
    bp.stages[0].model.models = vec![model_ref("open-model")];
    assert_eq!(entry_stage_window(&bp, &missing, None), 8192);
    std::fs::write(
        &config,
        "[model_capabilities.\"open-model\"]\nmax_context_tokens = 777\n",
    )
    .unwrap();
    assert_eq!(entry_stage_window(&bp, &config, None), 777);
}

/// `provider/model`, or a bare model, as a graph names it.
fn model_ref(text: &str) -> leviath_runtime::spec::names::ModelRef {
    leviath_runtime::spec::names::ModelRef::parse(text).unwrap()
}

/// A choice that would not fit the region's token budget stops the start
/// with an error naming the region, rather than a run that overflows.
#[test]
fn a_slot_over_budget_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(&dir.path().join("agents").join("looker"));
    let mut dash = dash_at(dir.path());
    let work = dir.path().join("work");
    std::fs::write(work.join("big.png"), b"\x89PNG\r\n\x1a\nbody").unwrap();
    dash.open_new_run_screen();
    // A tiny budget the ~1600-token image cannot fit.
    dash.new_run_inputs[0].max_tokens = 500;
    dash.new_run_inputs[0].files = vec![PathBuf::from("big.png")];
    let err = dash.new_run_input_values().unwrap_err();
    assert!(err.starts_with("pictures:"), "{err}");
    assert!(err.contains("region 'pictures' holds 500"), "{err}");
    // With room, it resolves.
    dash.new_run_inputs[0].max_tokens = 100000;
    assert!(dash.new_run_input_values().is_ok());
}

/// A chosen file that has gone missing stops the start with an error naming
/// the slot, rather than a run that began without it.
#[test]
fn a_missing_file_slot_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(&dir.path().join("agents").join("looker"));
    let mut dash = dash_at(dir.path());
    dash.open_new_run_screen();
    dash.new_run_inputs[0].files = vec![PathBuf::from("gone.png")];
    let err = dash.new_run_input_values().unwrap_err();
    assert!(err.starts_with("pictures:"), "{err}");
}

/// Inputs of every kind a row draws differently: a choice with a default, a
/// toggle, a bounded number, a list, a file, and the task the task box takes.
fn typed_decls() -> Vec<InputDecl> {
    let mut tone = decl(
        "tone",
        InputType::Choice {
            options: vec![
                ChoiceName::new("calm").unwrap(),
                ChoiceName::new("blunt").unwrap(),
            ],
        },
        false,
    );
    tone.default = Some(InputValue::Choice(ChoiceName::new("blunt").unwrap()));
    let mut deep = decl("deep", InputType::Bool, false);
    deep.default = Some(InputValue::Bool(true));
    let mut limit = decl(
        "limit",
        InputType::Int {
            min: Some(1),
            max: Some(5),
        },
        false,
    );
    limit.default = Some(InputValue::Int(3));
    vec![
        decl("task", text_type(), true),
        tone,
        decl("strict", InputType::Bool, true),
        limit,
        decl(
            "tags",
            InputType::List {
                item: Box::new(text_type()),
                min: None,
                max: Some(2),
            },
            false,
        ),
        decl(
            "cover",
            InputType::File {
                accepts: vec![MimePattern::new("image/*").unwrap()],
            },
            false,
        ),
        decl(
            "shots",
            InputType::List {
                item: Box::new(InputType::File {
                    accepts: Vec::new(),
                }),
                min: None,
                max: None,
            },
            false,
        ),
    ]
}

/// The looker agent's screen, its rows replaced by the typed ones.
fn typed_dash(dir: &Path) -> Dashboard {
    write_agent(&dir.join("agents").join("looker"));
    let mut dash = dash_at(dir);
    dash.open_new_run_screen();
    dash.new_run_inputs = rows_for(&typed_decls(), &[]);
    dash
}

/// Each input gets the row its type calls for, defaults filled in, and the
/// task is left to the task box. A text input takes its region's accepted
/// types, requirement and room.
#[test]
fn every_input_gets_a_row_for_its_type() {
    let rows = rows_for(&typed_decls(), &[]);
    let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
    assert_eq!(keys, ["tone", "strict", "limit", "tags", "cover", "shots"]);
    assert_eq!(rows[0].pick, Some(1), "the default option");
    assert_eq!(rows[1].pick, None, "a toggle with no default is not set");
    assert_eq!(rows[2].edit.value(), "3", "a default is pre-typed");
    assert!(!rows[2].required, "a default means it is never missing");
    assert_eq!(rows[4].accepts, ["image/*"]);
    assert!(rows[4].takes_files() && !rows[4].takes_text());
    assert!(rows[5].takes_files() && !rows[5].takes_text());
    assert!(!rows[0].takes_text() && !rows[0].takes_files());
    assert!(rows[3].takes_text() && !rows[3].takes_files());
    assert_eq!(rows[2].note(), " (an integer 1 to 5)");
    assert_eq!(rows[1].note(), " (true or false, required)");
    // A declared default that is not one of the options picks nothing.
    let mut odd = typed_decls()[1].clone();
    odd.default = Some(InputValue::Choice(ChoiceName::new("loud").unwrap()));
    assert_eq!(NewRunInput::new(&odd).pick, None);
    let mut deep = decl("deep", InputType::Bool, false);
    deep.default = Some(InputValue::Bool(true));
    assert_eq!(NewRunInput::new(&deep).pick, Some(1));

    // A text input bound to a region takes that region's settings.
    let mut notes = decl("notes", text_type(), false);
    notes.binds.push(InputSlot::OutputFormat);
    notes.binds.push(InputSlot::Region(
        leviath_runtime::spec::inputs::RegionBinding {
            region: leviath_runtime::spec::names::RegionName::new("pad").unwrap(),
            template: None,
        },
    ));
    let regions = vec![
        ("other".to_string(), Vec::new(), false, 0),
        ("pad".to_string(), vec!["image/*".to_string()], true, 900),
    ];
    let rows = rows_for(&[decl("depth", InputType::Bool, false), notes], &regions);
    assert_eq!(rows[1].key, "depth", "an input with no region comes last");
    let row = &rows[0];
    assert_eq!(row.region, "pad");
    assert_eq!(row.accepts, ["image/*"]);
    assert!(row.required);
    assert_eq!(row.max_tokens, 900);
}

/// A choice moves through its options with the arrows and Space, a toggle
/// flips, Enter moves on, and a number is typed.
#[test]
fn a_choice_cycles_a_toggle_flips_and_a_number_is_typed() {
    let dir = tempfile::tempdir().unwrap();
    let mut dash = typed_dash(dir.path());
    dash.new_run_focus = NewRunPane::Inputs;
    dash.new_run_input_selected = 0;
    dash.handle_new_run_key(key(KeyCode::Right));
    assert_eq!(dash.new_run_inputs[0].pick, Some(0), "wraps to the first");
    dash.handle_new_run_key(key(KeyCode::Left));
    assert_eq!(dash.new_run_inputs[0].pick, Some(1));
    dash.handle_new_run_key(key(KeyCode::Char(' ')));
    assert_eq!(dash.new_run_inputs[0].pick, Some(0));
    dash.handle_new_run_key(key(KeyCode::Char('x')));
    assert_eq!(
        dash.new_run_inputs[0].pick,
        Some(0),
        "a letter does nothing"
    );
    dash.handle_new_run_key(key(KeyCode::Enter));
    assert_eq!(dash.new_run_input_selected, 1);
    // A toggle not set starts on, then flips.
    dash.handle_new_run_key(key(KeyCode::Char(' ')));
    assert_eq!(dash.new_run_inputs[1].pick, Some(1));
    dash.handle_new_run_key(key(KeyCode::Right));
    assert_eq!(dash.new_run_inputs[1].pick, Some(0));
    dash.new_run_inputs[0].pick = None;
    dash.new_run_input_selected = 0;
    dash.handle_new_run_key(key(KeyCode::Left));
    assert_eq!(
        dash.new_run_inputs[0].pick,
        Some(0),
        "a choice starts first"
    );
    // The number row types.
    dash.new_run_input_selected = 2;
    dash.handle_new_run_key(key(KeyCode::Backspace));
    type_str(&mut dash, "4");
    assert_eq!(dash.new_run_inputs[2].edit.value(), "4");
    // Enter on a pick row that is the last moves on to the task.
    dash.new_run_inputs.truncate(2);
    dash.new_run_input_selected = 1;
    dash.handle_new_run_key(key(KeyCode::Enter));
    assert_eq!(dash.new_run_focus, NewRunPane::Task);
}

/// What the rows send: a choice's option, a toggle's state, a number as a
/// number, a list as a list, and a file input's files by name.
#[test]
fn typed_rows_resolve_to_typed_values() {
    let dir = tempfile::tempdir().unwrap();
    let mut dash = typed_dash(dir.path());
    let work = dir.path().join("work");
    std::fs::write(work.join("hero.png"), b"\x89PNG\r\n\x1a\nbody").unwrap();
    std::fs::write(work.join("a.txt"), "a").unwrap();
    dash.new_run_inputs[1].pick = Some(0);
    dash.new_run_inputs[3].edit = LineEdit::new("x, y", false);
    dash.new_run_inputs[4].files = vec![PathBuf::from("hero.png")];
    dash.new_run_inputs[5].files = vec![PathBuf::from("a.txt"), PathBuf::from("hero.png")];
    let got = dash.new_run_input_values().unwrap();
    assert_eq!(got.values.get("tone"), Some(&text("blunt")));
    assert_eq!(got.values.get("strict"), Some(&RawInput::Bool(false)));
    assert_eq!(got.values.get("limit"), Some(&RawInput::Int(3)));
    assert_eq!(
        got.values.get("tags"),
        Some(&RawInput::List(vec![text("x"), text("y")]))
    );
    assert_eq!(got.values.get("cover"), Some(&text("hero.png")));
    assert_eq!(
        got.values.get("shots"),
        Some(&RawInput::List(vec![text("a.txt"), text("hero.png")]))
    );
    assert!(got.parts.iter().all(|p| p.region.is_none()));
    // A choice with no options picks nothing to send.
    dash.new_run_inputs[0].ty = InputType::Choice {
        options: Vec::new(),
    };
    dash.new_run_inputs[0].pick = None;
    dash.new_run_inputs[0].cycle(true);
    assert_eq!(dash.new_run_inputs[0].pick, Some(0));
    assert!(
        !dash
            .new_run_input_values()
            .unwrap()
            .values
            .contains_key("tone")
    );
    // A file input with nothing chosen sends nothing; a number that is not
    // one stops the start, naming the row.
    dash.new_run_inputs[4].files.clear();
    assert!(
        !dash
            .new_run_input_values()
            .unwrap()
            .values
            .contains_key("cover")
    );
    dash.new_run_inputs[2].edit = LineEdit::new("lots", false);
    let err = dash.new_run_input_values().unwrap_err();
    assert!(err.starts_with("limit:"), "{err}");
}

/// Each input is checked against its declaration before anything is sent,
/// and a problem stays beside its row until the row is changed.
#[test]
fn a_problem_is_shown_beside_its_row_until_it_is_changed() {
    let dir = tempfile::tempdir().unwrap();
    let mut dash = typed_dash(dir.path());
    dash.new_run_inputs[2].edit = LineEdit::new("9", false);
    dash.new_run_inputs[3].edit = LineEdit::new("a,b,c", false);
    let resolved = dash.new_run_input_values().unwrap();
    // Out of range, a list too long, and a required toggle left unset.
    assert_eq!(dash.check_new_run_inputs(&resolved), 3);
    let issue = |dash: &Dashboard, i: usize| dash.new_run_inputs[i].issue.clone();
    assert!(
        issue(&dash, 1).unwrap().contains("required"),
        "{:?}",
        issue(&dash, 1)
    );
    assert!(issue(&dash, 2).unwrap().contains("out of range"));
    assert!(
        issue(&dash, 2)
            .unwrap()
            .contains("expected an integer 1 to 5")
    );
    assert!(issue(&dash, 3).unwrap().contains("the list has 3 items"));
    assert_eq!(dash.new_run_focus, NewRunPane::Inputs);
    assert_eq!(
        dash.new_run_input_selected, 1,
        "the first row with a problem"
    );
    let shown: String = dash.new_run_inputs[1]
        .value_spans(false)
        .iter()
        .map(|s| s.content.to_string())
        .collect();
    assert_eq!(shown, "[ ] not set  ✗ this input is required");
    // Changing a row clears its problem.
    dash.new_run_input_selected = 2;
    dash.handle_new_run_key(key(KeyCode::Backspace));
    assert!(dash.new_run_inputs[2].issue.is_none());
    dash.new_run_input_selected = 1;
    dash.handle_new_run_key(key(KeyCode::Char(' ')));
    assert!(dash.new_run_inputs[1].issue.is_none());
    // A required text input whose region got files has been given something.
    dash.new_run_inputs = vec![NewRunInput::new(&decl("pics", text_type(), true))];
    assert_eq!(dash.check_new_run_inputs(&ResolvedInputs::default()), 1);
    let given = ResolvedInputs {
        parts: vec![InboundPart::from_bytes("x.png", vec![1]).in_region("pics")],
        ..ResolvedInputs::default()
    };
    assert_eq!(dash.check_new_run_inputs(&given), 0);
}

/// The rows draw for their types: a picked option between arrows, a toggle
/// and its state, a prompt for what is not set yet, and a type hint.
#[test]
fn typed_rows_draw_for_their_types() {
    let dir = tempfile::tempdir().unwrap();
    let mut dash = typed_dash(dir.path());
    let drawn = screen(&mut dash);
    assert!(drawn.contains("‹ blunt ›"), "{drawn}");
    assert!(drawn.contains("[ ] not set"), "{drawn}");
    assert!(
        drawn.contains("a list of at most 2 items of text"),
        "{drawn}"
    );
    dash.new_run_inputs[1].pick = Some(1);
    dash.new_run_inputs[0].pick = None;
    let drawn = screen(&mut dash);
    assert!(drawn.contains("[x] yes"), "{drawn}");
    assert!(drawn.contains("not chosen"), "{drawn}");
    dash.new_run_inputs[1].pick = Some(0);
    dash.new_run_focus = NewRunPane::Inputs;
    dash.new_run_input_selected = 0;
    let drawn = screen(&mut dash);
    assert!(drawn.contains("[ ] no"), "{drawn}");
    assert!(drawn.contains("← → to choose"), "{drawn}");
}

/// Starting a run whose inputs do not check keeps the screen open with the
/// problems beside their rows; one that checks sends the typed values.
#[test]
fn a_run_with_a_bad_input_is_not_sent() {
    let dir = tempfile::tempdir().unwrap();
    let mut dash = typed_dash(dir.path());
    dash.new_run_task.area_mut().insert_str("look");
    dash.submit_new_run();
    assert!(dash.spawn_cmd_rx_for_test().try_recv().is_err());
    assert!(dash.new_run_screen);
    assert!(
        dash.toast_messages_for_test()
            .iter()
            .any(|t| t == "An input needs attention")
    );
    dash.new_run_inputs[2].edit = LineEdit::new("0", false);
    dash.submit_new_run();
    assert!(
        dash.toast_messages_for_test()
            .iter()
            .any(|t| t == "2 inputs need attention")
    );
    dash.new_run_inputs[1].pick = Some(1);
    dash.new_run_inputs[2].edit = LineEdit::new("2", false);
    dash.submit_new_run();
    let cmd = dash.spawn_cmd_rx_for_test().try_recv().unwrap();
    assert_eq!(cmd.values.get("strict"), Some(&RawInput::Bool(true)));
    assert_eq!(cmd.values.get("limit"), Some(&RawInput::Int(2)));
    assert!(!dash.new_run_screen);
}

/// A refusal from the daemon about an input brings the run back on the
/// new-run screen, task and all, with each problem beside its row; one about
/// nothing a row holds is a toast only.
#[test]
fn a_run_refused_for_an_input_comes_back_with_the_problem_beside_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut dash = typed_dash(dir.path());
    let path = dash.new_run_selected_agent().unwrap().path.clone();
    dash.new_run_inputs[3].edit = LineEdit::new("kept", false);
    dash.close_new_run_screen();
    let refusal = |issues: Vec<SpawnIssue>| SpawnOutcome {
        message: "refused".to_string(),
        refused: Some(RefusedRun {
            agent_path: path.clone(),
            task: "look again".to_string(),
            issues: SpawnIssues(issues),
        }),
        ..SpawnOutcome::default()
    };
    let at = SpecPath::root().field("inputs").key("limit");
    dash.inject_spawn_outcome_for_test(refusal(vec![
        SpawnIssue::new(at.clone(), IssueCode::OutOfRange, "too many")
            .expected("an integer 1 to 5"),
    ]));
    dash.drain_spawn_outcomes();
    assert!(dash.new_run_screen, "the screen opened again");
    assert_eq!(dash.new_run_selected_agent().unwrap().path, path);
    assert_eq!(dash.new_run_task.text(), "look again");
    assert_eq!(
        dash.new_run_inputs[3].edit.value(),
        "kept",
        "the rows were kept"
    );
    assert_eq!(
        dash.new_run_inputs[2].issue.as_deref(),
        Some("too many; expected an integer 1 to 5")
    );

    // A refusal about something no row holds is a toast, and the screen
    // stays shut.
    dash.close_new_run_screen();
    dash.inject_spawn_outcome_for_test(refusal(vec![SpawnIssue::new(
        SpecPath::root().field("model"),
        IssueCode::Invalid,
        "no such model",
    )]));
    dash.drain_spawn_outcomes();
    assert!(!dash.new_run_screen);
    assert!(dash.new_run_refused.is_none());

    // While the user is elsewhere it waits for the screen to open.
    dash.detail_view = true;
    dash.inject_spawn_outcome_for_test(refusal(vec![SpawnIssue::new(
        at,
        IssueCode::Missing,
        "required",
    )]));
    dash.drain_spawn_outcomes();
    assert!(!dash.new_run_screen);
    assert!(dash.new_run_refused.is_some());
    // An agent that has gone keeps the selection where it was, with fresh
    // rows; the problem still lands on a row of that name when there is one.
    dash.new_run_refused.as_mut().unwrap().agent_path = "gone".to_string();
    dash.new_run_inputs_key = "stale".to_string();
    dash.open_new_run_screen();
    assert!(dash.new_run_refused.is_none());
    assert_eq!(dash.new_run_task.text(), "look again");
    // A path that names no input is no row's.
    assert_eq!(issue_input(&SpecPath::root().field("inputs")), None);
    assert_eq!(
        issue_input(&SpecPath::root().field("stages").key("x")),
        None
    );
}

/// A bundled blueprint that is not installed still offers a row for every
/// input its `agent.toml` declares beside the task, each with its declared
/// type, so its typed inputs reach the widgets.
#[test]
fn a_bundled_blueprints_declared_inputs_become_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut offered = 0;
    for agent in crate::bundled::BUNDLED_AGENTS {
        let graph = super::super::graph::bundled_blueprint(agent.name).unwrap();
        let mut declared: Vec<(String, InputType)> = graph
            .inputs
            .iter()
            .filter(|d| d.name.as_str() != TASK_INPUT)
            .map(|d| (d.name.to_string(), d.ty.clone()))
            .collect();
        let mut dash = dash_at(dir.path());
        dash.last_launched_agent = Some(agent.name.to_string());
        dash.open_new_run_screen();
        assert_eq!(
            dash.new_run_selected_agent().map(|a| a.source.as_str()),
            Some("bundled"),
            "{}",
            agent.name
        );
        let mut rows: Vec<(String, InputType)> = dash
            .new_run_inputs
            .iter()
            .map(|r| (r.key.clone(), r.ty.clone()))
            .collect();
        declared.sort_by(|a, b| a.0.cmp(&b.0));
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(rows, declared, "{}", agent.name);
        offered += rows.len();
    }
    assert!(offered > 0, "some bundled blueprint declares an input");
}
