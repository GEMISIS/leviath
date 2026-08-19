//! The Agents screen and the editor, driven the way a person drives them:
//! keys and clicks against a temp home, frames rendered into a test
//! terminal, the file on disk read back.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::editor::{EditTarget, Focus, Overlay};
use super::inspector::{FieldId, Panel, StageTab};
use crate::blueprint_edit::{catalog, templates};
use crate::commands::dashboard::state::Dashboard;
use crate::commands::dashboard::test_support::{make_test_dashboard, rendered_buffer};
use crate::commands::dashboard::types::*;

pub(super) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::empty())
}

pub(super) fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

pub(super) fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::empty(),
    }
}

pub(super) fn type_str(dash: &mut Dashboard, text: &str) {
    for c in text.chars() {
        dash.handle_key(key(KeyCode::Char(c)));
    }
}

/// A dashboard whose agents directory is a fresh temp tree holding one
/// agent of our own (`own`, the starter) and the installed coder.
pub(super) fn dashboard(tag: &str) -> (Dashboard, PathBuf) {
    let root = std::env::temp_dir().join(format!("lev-agents-screen-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let agents = root.join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::create_dir_all(root.join("work")).unwrap();
    catalog::write_agent(&agents, "own", &templates::empty_blueprint("own").unwrap()).unwrap();
    catalog::reset_bundled(&agents, "coder").unwrap();
    let mut dash = make_test_dashboard();
    dash.new_run_ctx.agents_dir = agents;
    dash.new_run_ctx.workdir = root.join("work");
    dash.new_run_ctx.config_path = root.join("config.toml");
    dash.layout_store_path = Some(root.join("dash").join("graph-layouts.json"));
    (dash, root)
}

pub(super) fn draw(dash: &mut Dashboard, w: u16, h: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| dash.draw(f)).unwrap();
    terminal
}

pub(super) fn text(dash: &mut Dashboard) -> String {
    rendered_buffer(&draw(dash, 160, 50))
}

pub(super) fn open_editor_on(dash: &mut Dashboard, name: &str) {
    dash.handle_key(key(KeyCode::Char('a')));
    let at = dash
        .agents()
        .catalog
        .visible()
        .into_iter()
        .position(|i| dash.agents().catalog.entries[i].name == name)
        .expect("in the catalog");
    dash.agents().catalog.selected = at;
    dash.handle_key(key(KeyCode::Enter));
    assert!(
        dash.agents().editor.is_some(),
        "the editor opened on {name}"
    );
}

// ─── the catalog ─────────────────────────────────────────────────────────────

#[test]
fn a_opens_the_catalog_with_every_source_and_esc_closes_it() {
    let (mut dash, _root) = dashboard("catalog");
    dash.handle_key(key(KeyCode::Char('a')));
    assert!(dash.agent_builder.is_some());
    let screen = text(&mut dash);
    assert!(screen.contains("Agents ("), "{screen}");
    for name in ["own", "coder", "reviewer"] {
        assert!(screen.contains(name), "{name}: {screen}");
    }
    assert!(screen.contains("installed"), "{screen}");
    assert!(screen.contains("bundled"), "{screen}");
    // The first entry (coder, sorted) previews its graph and its about box.
    assert!(screen.contains("coder · v0.1.0 · 8 stages"), "{screen}");
    assert!(screen.contains("discover"), "{screen}");
    assert!(screen.contains("stages discover"), "{screen}");
    assert!(
        screen.contains("[enter] edit") || screen.contains("enter edit") || screen.contains("edit"),
        "{screen}"
    );
    // Move: the preview follows the cursor.
    dash.handle_key(key(KeyCode::Down));
    dash.handle_key(key(KeyCode::Char('j')));
    let screen = text(&mut dash);
    assert!(!screen.contains("coder · v0.1.0"), "{screen}");
    dash.handle_key(key(KeyCode::Char('k')));
    dash.handle_key(key(KeyCode::End));
    dash.handle_key(key(KeyCode::Home));
    dash.handle_key(key(KeyCode::PageDown));
    dash.handle_key(key(KeyCode::PageUp));
    dash.handle_key(key(KeyCode::Up));
    assert_eq!(dash.agents().catalog.selected, 0);
    // The filter: `/`, letters, enter keeps it, esc clears it.
    dash.handle_key(key(KeyCode::Char('/')));
    type_str(&mut dash, "reviewerx");
    dash.handle_key(key(KeyCode::Backspace));
    let screen = text(&mut dash);
    assert!(screen.contains("/reviewer▌"), "{screen}");
    assert_eq!(dash.agents().catalog.visible().len(), 1, "reviewer");
    dash.handle_key(key(KeyCode::Tab)); // ignored while filtering
    dash.handle_key(key(KeyCode::Enter));
    assert!(!dash.agents().catalog.filtering);
    assert!(text(&mut dash).contains("/reviewer  1/"), "kept");
    dash.handle_key(key(KeyCode::Esc));
    assert!(
        dash.agents().catalog.filter.is_empty(),
        "esc clears the filter first"
    );
    dash.handle_key(key(KeyCode::Char('/')));
    type_str(&mut dash, "zzz");
    assert!(
        text(&mut dash).contains("No agents match"),
        "{}",
        text(&mut dash)
    );
    dash.handle_key(key(KeyCode::Esc));
    assert!(dash.agents().catalog.filter.is_empty() && !dash.agents().catalog.filtering);
    dash.handle_key(key(KeyCode::Char('?')));
    assert!(dash.show_help);
    assert!(text(&mut dash).contains("Agents (a)"));
    dash.handle_key(key(KeyCode::Esc));
    dash.handle_key(key(KeyCode::Char('q')));
    assert!(dash.agent_builder.is_none(), "q closes the screen");
    // An unknown key is nothing.
    dash.handle_key(key(KeyCode::Char('a')));
    dash.handle_key(key(KeyCode::Char('z')));
    assert!(dash.agent_builder.is_some());
    // The main list's help bar and overlay name the screen.
    dash.handle_key(key(KeyCode::Esc));
    let screen = text(&mut dash);
    assert!(screen.contains("[a] agents"), "{screen}");
}

#[test]
fn an_empty_home_says_so_and_a_broken_manifest_declines_the_preview() {
    let (mut dash, root) = dashboard("empty");
    let _ = std::fs::remove_dir_all(root.join("agents"));
    std::fs::create_dir_all(root.join("agents").join("broken")).unwrap();
    std::fs::write(
        root.join("agents").join("broken").join("agent.leviath"),
        "[agent]\nname = \"broken\"\n[stages.a]\nmode = \"weird\"\n",
    )
    .unwrap();
    dash.handle_key(key(KeyCode::Char('a')));
    // The bundled ones are still there; a broken installed one is not
    // listed by the catalog (it cannot be parsed).
    let screen = text(&mut dash);
    assert!(screen.contains("coder"), "{screen}");
    // Filter to nothing: the list says so, and the detail pane asks.
    dash.handle_key(key(KeyCode::Char('/')));
    type_str(&mut dash, "nothing-here");
    dash.handle_key(key(KeyCode::Enter));
    let screen = text(&mut dash);
    assert!(screen.contains("No agents match"), "{screen}");
    assert!(screen.contains("Pick an agent to see it."), "{screen}");
    // Actions on nothing do nothing.
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::Char('d')));
    dash.handle_key(key(KeyCode::Char('r')));
    dash.handle_key(key(KeyCode::Char('l')));
    assert!(dash.agent_builder.is_some() && dash.agents().editor.is_none());
    // A preview that cannot be built says why (an unreadable manifest, a
    // manifest the runtime rejects); the second still opens in the editor,
    // which edits TOML, not the runtime's view of it.
    dash.agents().catalog.filter.clear();
    let odd = catalog::CatalogEntry {
        name: "odd".into(),
        version: "0".into(),
        description: String::new(),
        source: catalog::Source::Installed,
        dir: None,
        manifest: Some("[agent]\nname = \"odd\"\n[stages.a]\nmode = \"weird\"\n".into()),
        stages: vec![],
        bundled: false,
        differs_from_bundled: false,
    };
    let unread = catalog::CatalogEntry {
        name: "unread".into(),
        manifest: None,
        ..odd.clone()
    };
    dash.agents().catalog.entries.push(unread);
    let n = dash.agents().catalog.entries.len();
    dash.agents().catalog.selected = n - 1;
    let screen = text(&mut dash);
    assert!(screen.contains("could not be read"), "{screen}");
    dash.handle_key(key(KeyCode::Enter));
    assert!(dash.agents().editor.is_none(), "nothing to edit");
    assert!(
        dash.toasts
            .iter()
            .any(|t| t.message.contains("could not be read"))
    );
    dash.agents().catalog.entries.push(odd);
    dash.agents().catalog.selected = n;
    let screen = text(&mut dash);
    assert!(screen.contains("mode"), "the runtime's complaint: {screen}");
    dash.handle_key(key(KeyCode::Enter));
    assert!(dash.agents().editor.is_some());
    dash.handle_key(key(KeyCode::Esc));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn delete_and_reset_ask_first_and_launch_goes_to_the_new_run_screen() {
    let (mut dash, root) = dashboard("actions");
    // Edit the installed coder so it differs from the bundle.
    let manifest = root.join("agents").join("coder").join("agent.leviath");
    let edited = std::fs::read_to_string(&manifest)
        .unwrap()
        .replace("entry_stage = \"discover\"", "entry_stage = \"plan\"");
    std::fs::write(&manifest, edited).unwrap();
    dash.handle_key(key(KeyCode::Char('a')));
    let screen = text(&mut dash);
    assert!(screen.contains("edited"), "{screen}");
    assert!(screen.contains("puts the bundled copy back"), "{screen}");
    // Reset: No keeps it, Yes restores it. The dialog draws over the screen.
    dash.handle_key(key(KeyCode::Char('r')));
    assert!(text(&mut dash).contains("Reset to original?"));
    dash.tick_graphs(std::time::Duration::from_millis(100));
    assert!(matches!(
        dash.pending_confirm,
        Some((ConfirmAction::AgentReset { .. }, _))
    ));
    dash.handle_key(key(KeyCode::Char('n')));
    assert!(dash.pending_confirm.is_none());
    dash.handle_key(key(KeyCode::Char('r')));
    dash.handle_key(key(KeyCode::Char('y')));
    assert!(
        std::fs::read_to_string(&manifest)
            .unwrap()
            .contains("entry_stage = \"discover\"")
    );
    assert!(
        dash.toasts
            .iter()
            .any(|t| t.message.contains("Reset coder"))
    );
    // Reset on an agent that is not an edited bundled one is a toast.
    dash.handle_key(key(KeyCode::Char('r')));
    assert!(dash.pending_confirm.is_none());
    assert!(
        dash.toasts
            .iter()
            .any(|t| t.message.contains("nothing to reset"))
    );
    // Delete: only what lives under the agents dir.
    let own = dash
        .agents()
        .catalog
        .visible()
        .into_iter()
        .position(|i| dash.agents().catalog.entries[i].name == "own")
        .unwrap();
    dash.agents().catalog.selected = own;
    dash.handle_key(key(KeyCode::Char('d')));
    assert!(matches!(
        dash.pending_confirm,
        Some((ConfirmAction::AgentDelete { .. }, _))
    ));
    dash.handle_key(key(KeyCode::Char('y')));
    assert!(!root.join("agents").join("own").exists());
    assert!(
        !dash
            .agents()
            .catalog
            .entries
            .iter()
            .any(|e| e.name == "own")
    );
    // A bundled-not-installed one is not deletable.
    let reviewer = dash
        .agents()
        .catalog
        .visible()
        .into_iter()
        .position(|i| dash.agents().catalog.entries[i].name == "reviewer")
        .unwrap();
    dash.agents().catalog.selected = reviewer;
    dash.handle_key(key(KeyCode::Char('d')));
    assert!(dash.pending_confirm.is_none());
    assert!(
        dash.toasts
            .iter()
            .any(|t| t.message.contains("not deletable"))
    );
    // Nor is one that lives elsewhere.
    dash.agents().catalog.entries[0].source = catalog::Source::Configured;
    dash.agents().catalog.selected = 0;
    dash.handle_key(key(KeyCode::Char('d')));
    assert!(
        dash.toasts
            .iter()
            .any(|t| t.message.contains("delete it where it is"))
    );
    // A delete that fails on disk is a toast too.
    dash.perform_agent_delete("ghost");
    assert!(
        dash.toasts
            .iter()
            .any(|t| t.message.contains("Could not delete ghost"))
    );
    dash.perform_agent_reset("own");
    assert!(
        dash.toasts
            .iter()
            .any(|t| t.message.contains("Could not reset own"))
    );
    // Launch: the new-run screen opens with the agent picked.
    dash.agents().catalog.selected = reviewer;
    dash.handle_key(key(KeyCode::Char('l')));
    assert!(dash.agent_builder.is_none());
    assert!(dash.new_run_screen);
    assert_eq!(dash.new_run_agents[dash.new_run_selected].name, "reviewer");
    let _ = std::fs::remove_dir_all(&root);
}

// ─── the chooser ─────────────────────────────────────────────────────────────

#[test]
fn the_chooser_starts_simple_or_clones_and_checks_the_name() {
    let (mut dash, root) = dashboard("chooser");
    dash.handle_key(key(KeyCode::Char('a')));
    dash.handle_key(key(KeyCode::Char('n')));
    assert!(dash.agents().chooser.is_some());
    let screen = text(&mut dash);
    assert!(screen.contains("New agent"), "{screen}");
    assert!(screen.contains("Start simple"), "{screen}");
    assert!(screen.contains("Clone coder"), "{screen}");
    assert!(screen.contains("Name  my-agent"), "{screen}");
    // The name follows the row until typed into.
    dash.handle_key(key(KeyCode::Down));
    assert!(
        dash.agents()
            .chooser
            .as_ref()
            .unwrap()
            .name
            .value()
            .starts_with("my-")
    );
    dash.handle_key(key(KeyCode::Up));
    assert_eq!(
        dash.agents().chooser.as_ref().unwrap().name.value(),
        "my-agent"
    );
    // A taken name, a bad name: said on the line, Enter refused.
    for _ in 0..12 {
        dash.handle_key(key(KeyCode::Backspace));
    }
    type_str(&mut dash, "own");
    assert!(text(&mut dash).contains("already exists"));
    dash.handle_key(key(KeyCode::Enter));
    assert!(dash.agents().chooser.is_some(), "refused");
    dash.handle_key(key(KeyCode::Backspace));
    dash.handle_key(key(KeyCode::Backspace));
    dash.handle_key(key(KeyCode::Backspace));
    type_str(&mut dash, "no way");
    assert!(text(&mut dash).contains("Letters, digits"));
    dash.handle_key(key(KeyCode::Enter));
    assert!(dash.agents().chooser.is_some());
    // The row no longer changes the name once typed.
    dash.handle_key(key(KeyCode::Down));
    assert_eq!(
        dash.agents().chooser.as_ref().unwrap().name.value(),
        "no way"
    );
    dash.handle_key(key(KeyCode::Esc));
    assert!(dash.agents().chooser.is_none());
    // Start simple under a good name opens the editor, unsaved.
    dash.handle_key(key(KeyCode::Char('n')));
    for _ in 0..12 {
        dash.handle_key(key(KeyCode::Backspace));
    }
    type_str(&mut dash, "demo");
    dash.handle_key(key(KeyCode::Enter));
    let editor = dash.agents().editor.as_ref().unwrap();
    assert!(editor.is_new && editor.dirty);
    assert_eq!(editor.name, "demo");
    assert_eq!(editor.doc.stage_names(), ["work", "finish"]);
    assert!(text(&mut dash).contains("(not saved yet)"));
    // Close it (dirty: asks; discard) and clone the coder instead.
    dash.handle_key(key(KeyCode::Esc));
    assert!(matches!(
        dash.pending_confirm,
        Some((ConfirmAction::EditorDiscard, _))
    ));
    dash.handle_key(key(KeyCode::Char('y')));
    assert!(dash.agents().editor.is_none());
    dash.handle_key(key(KeyCode::Char('n')));
    dash.handle_key(key(KeyCode::Down)); // clone coder (sorted first)
    dash.handle_key(key(KeyCode::Enter));
    let editor = dash.agents().editor.as_ref().unwrap();
    assert_eq!(editor.name, "my-coder");
    assert!(editor.doc.has_stage("discover"));
    // Save: the clone lands under the agents dir with the coder's scripts
    // (it has none; the reviewer neither), and is no longer new.
    dash.handle_key(ctrl('s'));
    assert!(
        root.join("agents")
            .join("my-coder")
            .join("agent.leviath")
            .exists()
    );
    let editor = dash.agents().editor.as_ref().unwrap();
    assert!(!editor.is_new && !editor.dirty);
    assert!(text(&mut dash).contains("Saved to"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_clone_of_a_bundled_agent_with_scripts_takes_them_along() {
    let (mut dash, root) = dashboard("clone-scripts");
    dash.handle_key(key(KeyCode::Char('a')));
    dash.handle_key(key(KeyCode::Char('n')));
    let at = dash
        .agents()
        .chooser
        .as_ref()
        .unwrap()
        .templates
        .iter()
        .position(|t| t.label == "Clone researcher")
        .unwrap();
    for _ in 0..at {
        dash.handle_key(key(KeyCode::Down));
    }
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(dash.agents().editor.as_ref().unwrap().name, "my-researcher");
    dash.handle_key(ctrl('s'));
    assert!(
        root.join("agents")
            .join("my-researcher")
            .join("tools")
            .join("web_search.rhai")
            .exists()
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ─── the editor ──────────────────────────────────────────────────────────────

#[test]
fn the_canvas_adds_connects_selects_and_deletes_with_undo_behind_it() {
    let (mut dash, root) = dashboard("canvas");
    open_editor_on(&mut dash, "own");
    let screen = text(&mut dash);
    assert!(screen.contains("Agent editor · own"), "{screen}");
    assert!(screen.contains("This agent"), "{screen}");
    assert!(screen.contains("[hint]"), "the path is labelled: {screen}");
    assert!(
        screen.contains("no problems") || screen.contains("warning"),
        "{screen}"
    );
    // Select the entry stage with the arrows; Enter opens the inspector.
    dash.handle_key(key(KeyCode::Right));
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::Stage {
            name: "work".into(),
            tab: StageTab::Behaviour
        }
    );
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().focus,
        Focus::Inspector
    );
    assert!(text(&mut dash).contains("Stage · work · Behaviour"));
    dash.handle_key(key(KeyCode::Esc));
    assert_eq!(dash.agents().editor.as_ref().unwrap().focus, Focus::Canvas);
    // Add a stage after work.
    dash.handle_key(key(KeyCode::Char('a')));
    assert!(dash.agents().editor.as_ref().unwrap().add_stage.is_some());
    assert!(text(&mut dash).contains("New stage"));
    type_str(&mut dash, "review");
    dash.handle_key(key(KeyCode::Enter));
    let editor = dash.agents().editor.as_ref().unwrap();
    assert_eq!(editor.doc.stage_names(), ["work", "review", "finish"]);
    assert_eq!(
        editor.panel,
        Panel::Stage {
            name: "review".into(),
            tab: StageTab::Behaviour
        }
    );
    assert_eq!(editor.focus, Focus::Inspector);
    assert!(editor.dirty);
    // An empty name is ignored; Esc cancels the prompt; a taken name is
    // refused with a message.
    dash.handle_key(key(KeyCode::Esc));
    dash.handle_key(key(KeyCode::Char('a')));
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage_names()
            .len(),
        3
    );
    dash.handle_key(key(KeyCode::Char('a')));
    dash.handle_key(key(KeyCode::Esc));
    assert!(dash.agents().editor.as_ref().unwrap().add_stage.is_none());
    dash.handle_key(key(KeyCode::Char('a')));
    type_str(&mut dash, "work");
    dash.handle_key(key(KeyCode::Enter));
    assert!(text(&mut dash).contains("already taken"));
    // Connect review to finish through the chooser (typing narrows it).
    dash.handle_key(key(KeyCode::Char('c')));
    assert!(text(&mut dash).contains("Connect review to"));
    type_str(&mut dash, "fin");
    dash.handle_key(key(KeyCode::Enter));
    let editor = dash.agents().editor.as_ref().unwrap();
    assert!(editor.doc.edge("review", "finish").is_some());
    assert_eq!(
        editor.panel,
        Panel::Edge {
            from: "review".into(),
            to: "finish".into()
        }
    );
    assert!(text(&mut dash).contains("Path · review → finish"));
    // A loop to itself.
    dash.handle_key(key(KeyCode::Esc));
    dash.handle_key(key(KeyCode::Char('[')));
    dash.handle_key(key(KeyCode::Char(']')));
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .view
        .select_stage("review");
    dash.agents().editor.as_mut().unwrap().sync_panel();
    dash.handle_key(key(KeyCode::Char('c')));
    type_str(&mut dash, "review");
    dash.handle_key(key(KeyCode::Enter));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .edge("review", "review")
            .is_some()
    );
    assert!(text(&mut dash).contains("↺ loops"));
    // The loop opened its own path panel; Esc goes back to the stage, a
    // second Esc to the canvas.
    assert!(matches!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::Edge { .. }
    ));
    dash.handle_key(key(KeyCode::Esc));
    assert!(matches!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::Stage { .. }
    ));
    // Connect from nothing selected: a message.
    dash.handle_key(key(KeyCode::Esc));
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .view
        .clear_selection();
    dash.agents().editor.as_mut().unwrap().sync_panel();
    dash.handle_key(key(KeyCode::Char('c')));
    assert!(dash.agents().editor.as_ref().unwrap().picker.is_none());
    assert!(text(&mut dash).contains("Select a stage to connect from"));
    dash.handle_key(key(KeyCode::Char('x')));
    assert!(text(&mut dash).contains("Select a stage or a path to delete"));
    // Delete the path with x, then undo and redo it.
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .view
        .select_edge("review", "finish");
    dash.agents().editor.as_mut().unwrap().sync_panel();
    dash.handle_key(key(KeyCode::Char('x')));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .edge("review", "finish")
            .is_none()
    );
    dash.handle_key(key(KeyCode::Char('u')));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .edge("review", "finish")
            .is_some()
    );
    dash.handle_key(ctrl('r'));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .edge("review", "finish")
            .is_none()
    );
    dash.handle_key(ctrl('r'));
    assert!(text(&mut dash).contains("Nothing to redo"));
    // Delete a stage: asks; No keeps it, Yes removes it and its paths.
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .view
        .select_stage("review");
    dash.agents().editor.as_mut().unwrap().sync_panel();
    dash.handle_key(key(KeyCode::Delete));
    assert!(matches!(
        dash.pending_confirm,
        Some((ConfirmAction::StageDelete { .. }, _))
    ));
    dash.handle_key(key(KeyCode::Char('n')));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .has_stage("review")
    );
    dash.handle_key(key(KeyCode::Char('x')));
    dash.handle_key(key(KeyCode::Char('y')));
    let editor = dash.agents().editor.as_ref().unwrap();
    assert!(!editor.doc.has_stage("review"));
    assert_eq!(editor.panel, Panel::Agent);
    // Undo everything: the file is what it was, and one more undo says so.
    while dash.agents().editor.as_mut().unwrap().undo() {}
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().doc.stage_names(),
        ["work", "finish"]
    );
    dash.handle_key(key(KeyCode::Char('u')));
    assert!(text(&mut dash).contains("Nothing to undo"));
    // The rest of the canvas keys: rotate, fit, zoom, and a key nobody has.
    for code in [
        KeyCode::Char('r'),
        KeyCode::Char('f'),
        KeyCode::Char('+'),
        KeyCode::Char('-'),
        KeyCode::Char('0'),
        KeyCode::Char('z'),
    ] {
        dash.handle_key(key(code));
    }
    assert!(dash.agents().editor.is_some());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_inspector_edits_every_kind_of_field() {
    let (mut dash, root) = dashboard("inspector");
    open_editor_on(&mut dash, "own");
    // The agent panel: description (text), starts at (choice), model.
    dash.handle_key(key(KeyCode::Tab));
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().focus,
        Focus::Inspector
    );
    dash.handle_key(key(KeyCode::Enter)); // description
    assert!(dash.agents().editor.as_ref().unwrap().line.is_some());
    for _ in 0..40 {
        dash.handle_key(key(KeyCode::Backspace));
    }
    type_str(&mut dash, "Builds things");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .agent()
            .description,
        "Builds things"
    );
    dash.handle_key(key(KeyCode::Down)); // starts at
    dash.handle_key(key(KeyCode::Right));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .agent()
            .entry_stage
            .as_deref(),
        Some("finish")
    );
    dash.handle_key(key(KeyCode::Left));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .agent()
            .entry_stage
            .as_deref(),
        Some("work")
    );
    dash.handle_key(key(KeyCode::Enter)); // the chooser
    assert!(dash.agents().editor.as_ref().unwrap().picker.is_some());
    assert!(text(&mut dash).contains("Starts at"));
    dash.handle_key(key(KeyCode::Down));
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .agent()
            .entry_stage
            .as_deref(),
        Some("finish")
    );
    dash.handle_key(key(KeyCode::Down)); // default model
    dash.handle_key(key(KeyCode::Enter));
    assert!(text(&mut dash).contains("Default model"));
    type_str(&mut dash, "claude");
    dash.handle_key(key(KeyCode::Enter));
    let model = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .doc
        .agent()
        .default_model
        .clone();
    assert!(
        model.as_deref().is_some_and(|m| m.contains("claude")),
        "{model:?}"
    );
    // Left/right on the model cycles the catalog too; Esc in a chooser
    // cancels; a region row is inert.
    dash.handle_key(key(KeyCode::Right));
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::Esc));
    assert!(dash.agents().editor.as_ref().unwrap().picker.is_none());
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .add_region(&crate::blueprint_edit::RegionScope::Shared, "notes")
        .unwrap();
    dash.agents().editor.as_mut().unwrap().refresh();
    dash.handle_key(key(KeyCode::End));
    let field = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .current_field()
        .unwrap();
    assert_eq!(field.id, FieldId::RegionRow("notes".into()));
    // Enter opens the region's own panel; Esc comes back to the agent.
    dash.handle_key(key(KeyCode::Enter));
    assert!(matches!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::Region { .. }
    ));
    dash.handle_key(key(KeyCode::Esc));
    assert!(text(&mut dash).contains("Shared region"));
    dash.handle_key(key(KeyCode::Home));
    // The stage panel.
    dash.handle_key(key(KeyCode::Tab));
    dash.handle_key(key(KeyCode::Right)); // work
    dash.handle_key(key(KeyCode::Enter));
    let fields = dash.agents().editor.as_ref().unwrap().fields();
    let at = |id: FieldId| fields.iter().position(|f| f.id == id).unwrap();
    // Rename.
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "2");
    dash.handle_key(key(KeyCode::Enter));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .has_stage("work2")
    );
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::Stage {
            name: "work2".into(),
            tab: StageTab::Behaviour
        }
    );
    // Mode: cycle with the arrows, then through the chooser to fan-out.
    dash.agents().editor.as_mut().unwrap().cursor = at(FieldId::StageMode);
    dash.handle_key(key(KeyCode::Right));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work2")
            .unwrap()
            .mode,
        crate::blueprint_edit::StageModeView::InteractivePoints
    );
    dash.handle_key(key(KeyCode::Left));
    dash.handle_key(key(KeyCode::Left));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work2")
            .unwrap()
            .mode,
        crate::blueprint_edit::StageModeView::Output
    );
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "fan");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work2")
            .unwrap()
            .mode,
        crate::blueprint_edit::StageModeView::FanOut
    );
    // The fan-out rows are live now: worker kind, worker, merge, caps, policy.
    dash.agents().editor.as_mut().unwrap().cursor = at(FieldId::WorkerKind);
    dash.handle_key(key(KeyCode::Right));
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::Enter));
    dash.agents().editor.as_mut().unwrap().cursor = at(FieldId::WorkerRef);
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "finish");
    dash.handle_key(key(KeyCode::Enter));
    let fan = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .doc
        .stage("work2")
        .unwrap()
        .fan_out;
    assert_eq!(fan.worker.as_ref().map(|(_, v)| v.as_str()), Some("finish"));
    dash.agents().editor.as_mut().unwrap().cursor = at(FieldId::MergeStage);
    dash.handle_key(key(KeyCode::Right));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work2")
            .unwrap()
            .fan_out
            .merge_stage
            .as_deref(),
        Some("finish")
    );
    dash.handle_key(key(KeyCode::Left));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work2")
            .unwrap()
            .fan_out
            .merge_stage,
        None
    );
    dash.agents().editor.as_mut().unwrap().cursor = at(FieldId::MaxWorkers);
    dash.handle_key(key(KeyCode::Right));
    dash.handle_key(key(KeyCode::Right));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work2")
            .unwrap()
            .fan_out
            .max_workers,
        Some(2)
    );
    dash.handle_key(key(KeyCode::Left));
    dash.handle_key(key(KeyCode::Left));
    dash.handle_key(key(KeyCode::Left));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work2")
            .unwrap()
            .fan_out
            .max_workers,
        None
    );
    dash.handle_key(key(KeyCode::Left));
    dash.agents().editor.as_mut().unwrap().cursor = at(FieldId::MaxItems);
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "7");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work2")
            .unwrap()
            .fan_out
            .max_items,
        Some(7)
    );
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "x");
    dash.handle_key(key(KeyCode::Enter));
    assert!(text(&mut dash).contains("is not a whole number"));
    dash.agents().editor.as_mut().unwrap().cursor = at(FieldId::OnWorkerFailure);
    dash.handle_key(key(KeyCode::Right));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work2")
            .unwrap()
            .fan_out
            .on_worker_failure
            .as_deref(),
        Some("fail_all")
    );
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::Enter));
    // Clearing the worker.
    dash.agents().editor.as_mut().unwrap().cursor = at(FieldId::WorkerRef);
    dash.handle_key(key(KeyCode::Enter));
    for _ in 0..8 {
        dash.handle_key(key(KeyCode::Backspace));
    }
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work2")
            .unwrap()
            .fan_out
            .worker,
        None
    );
    dash.agents().editor.as_mut().unwrap().cursor = at(FieldId::WorkerKind);
    dash.handle_key(key(KeyCode::Right));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work2")
            .unwrap()
            .fan_out
            .worker
            .is_some()
    );
    // Back to autonomous: the fan-out rows are disabled and inert.
    dash.agents().editor.as_mut().unwrap().cursor = at(FieldId::StageMode);
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::Home));
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work2")
            .unwrap()
            .mode,
        crate::blueprint_edit::StageModeView::Autonomous
    );
    dash.agents().editor.as_mut().unwrap().cursor = at(FieldId::MaxWorkers);
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::Right));
    assert!(dash.agents().editor.as_ref().unwrap().line.is_none());
    // Description, tries, revisits, allow complete.
    dash.agents().editor.as_mut().unwrap().cursor = at(FieldId::StageDescription);
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "!");
    dash.handle_key(key(KeyCode::Enter));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work2")
            .unwrap()
            .description
            .ends_with('!')
    );
    dash.agents().editor.as_mut().unwrap().cursor = at(FieldId::MaxIterations);
    dash.handle_key(key(KeyCode::Right));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work2")
            .unwrap()
            .max_iterations,
        Some(26)
    );
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::Backspace));
    dash.handle_key(key(KeyCode::Backspace));
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work2")
            .unwrap()
            .max_iterations,
        None
    );
    dash.agents().editor.as_mut().unwrap().cursor = at(FieldId::MaxRevisits);
    dash.handle_key(key(KeyCode::Right));
    dash.handle_key(key(KeyCode::Right));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work2")
            .unwrap()
            .max_revisits,
        Some(2)
    );
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "0");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work2")
            .unwrap()
            .max_revisits,
        Some(20)
    );
    dash.agents().editor.as_mut().unwrap().cursor = at(FieldId::AllowComplete);
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work2")
            .unwrap()
            .allow_complete,
        Some(true)
    );
    dash.handle_key(key(KeyCode::Right));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work2")
            .unwrap()
            .allow_complete,
        None
    );
    // Move down, then up; the tabs; the delete button (asks).
    dash.agents().editor.as_mut().unwrap().cursor = at(FieldId::MoveDown);
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().doc.stage_names(),
        ["finish", "work2"]
    );
    dash.agents().editor.as_mut().unwrap().cursor = at(FieldId::MoveUp);
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().doc.stage_names(),
        ["work2", "finish"]
    );
    dash.handle_key(key(KeyCode::Char('2')));
    assert!(text(&mut dash).contains("Model & tools"));
    dash.handle_key(key(KeyCode::Char('3')));
    dash.handle_key(key(KeyCode::Char('1')));
    dash.handle_key(key(KeyCode::Char('j')));
    dash.handle_key(key(KeyCode::Char('k')));
    dash.agents().editor.as_mut().unwrap().cursor = at(FieldId::DeleteStage);
    dash.handle_key(key(KeyCode::Enter));
    assert!(matches!(
        dash.pending_confirm,
        Some((ConfirmAction::StageDelete { .. }, _))
    ));
    dash.handle_key(key(KeyCode::Char('n')));
    // The path panel: kind, hint, gate, delete.
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .view
        .select_edge("work2", "finish");
    dash.agents().editor.as_mut().unwrap().sync_panel();
    dash.agents().editor.as_mut().unwrap().cursor = 0;
    dash.handle_key(key(KeyCode::Right));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .edge("work2", "finish")
            .unwrap()
            .kind,
        crate::blueprint_edit::EdgeKind::LlmChoice
    );
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "always");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .edge("work2", "finish")
            .unwrap()
            .kind,
        crate::blueprint_edit::EdgeKind::Always
    );
    // The hint row is disabled under a condition; back to hint it edits.
    dash.handle_key(key(KeyCode::Down));
    dash.handle_key(key(KeyCode::Enter));
    assert!(dash.agents().editor.as_ref().unwrap().line.is_none());
    dash.handle_key(key(KeyCode::Up));
    dash.handle_key(key(KeyCode::Right));
    dash.handle_key(key(KeyCode::Down));
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "!");
    dash.handle_key(key(KeyCode::Enter));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .edge("work2", "finish")
            .unwrap()
            .hint
            .as_deref()
            .unwrap()
            .ends_with('!')
    );
    dash.handle_key(key(KeyCode::Down));
    dash.handle_key(key(KeyCode::Enter));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .edge("work2", "finish")
            .unwrap()
            .gated
    );
    dash.handle_key(key(KeyCode::Left));
    assert!(
        !dash
            .agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .edge("work2", "finish")
            .unwrap()
            .gated
    );
    // The last row is the delete button.
    dash.handle_key(key(KeyCode::End));
    dash.handle_key(key(KeyCode::Enter));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .edge("work2", "finish")
            .is_none()
    );
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::Stage {
            name: "work2".into(),
            tab: StageTab::Behaviour
        }
    );
    // The delete-stage button on the last stage is disabled.
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .delete_stage("finish")
        .unwrap();
    dash.agents().editor.as_mut().unwrap().refresh();
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .view
        .select_stage("work2");
    dash.agents().editor.as_mut().unwrap().sync_panel();
    let fields = dash.agents().editor.as_ref().unwrap().fields();
    assert!(
        !fields
            .iter()
            .find(|f| f.id == FieldId::DeleteStage)
            .unwrap()
            .enabled
    );
    dash.editor_request_delete_stage("work2");
    assert!(dash.pending_confirm.is_none());
    assert!(text(&mut dash).contains("only stage"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn saving_checks_first_and_writes_the_file_and_the_layout() {
    let (mut dash, root) = dashboard("save");
    open_editor_on(&mut dash, "own");
    // A tool nobody has is a lint error: the save is refused and the
    // problems open.
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .set_tools("work", &["no_such_tool".into()])
        .unwrap();
    dash.agents().editor.as_mut().unwrap().refresh();
    dash.handle_key(ctrl('s'));
    let editor = dash.agents().editor.as_ref().unwrap();
    assert!(
        editor.dirty
            || editor
                .message
                .as_deref()
                .is_some_and(|m| m.contains("Not saved"))
    );
    assert!(editor.problems_open);
    let screen = text(&mut dash);
    assert!(screen.contains("Not saved: 1 problem"), "{screen}");
    assert!(
        screen.contains("unknown-tool") || screen.contains("no_such_tool"),
        "{screen}"
    );
    assert!(
        screen.contains("! ") && screen.contains("error"),
        "the box is flagged: {screen}"
    );
    // `p` folds the list; fix it; save writes.
    dash.handle_key(key(KeyCode::Char('p')));
    assert!(!dash.agents().editor.as_ref().unwrap().problems_open);
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .set_tools("work", &["read_file".into()])
        .unwrap();
    dash.agents().editor.as_mut().unwrap().refresh();
    // Move a box so the layout has something to remember.
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .view
        .select_stage("work");
    dash.handle_key(ctrl('s'));
    let text_on_disk =
        std::fs::read_to_string(root.join("agents").join("own").join("agent.leviath")).unwrap();
    assert!(
        text_on_disk.contains("available_tools = [\"read_file\"]"),
        "{text_on_disk}"
    );
    assert!(root.join("dash").join("graph-layouts.json").exists());
    let editor = dash.agents().editor.as_ref().unwrap();
    assert!(!editor.dirty);
    assert!(dash.toasts.iter().any(|t| t.message == "Saved own"));
    // A save into a directory that cannot be written says so.
    dash.agents().editor.as_mut().unwrap().dir = root
        .join("agents")
        .join("own")
        .join("agent.leviath")
        .join("x");
    dash.handle_key(ctrl('s'));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .message
            .as_deref()
            .unwrap()
            .contains("Could not write")
    );
    // Not dirty: Esc closes without asking, and the catalog is back.
    dash.agents().editor.as_mut().unwrap().dirty = false;
    dash.handle_key(key(KeyCode::Esc));
    assert!(dash.agent_builder.as_ref().unwrap().editor.is_none());
    assert!(text(&mut dash).contains("Agents ("));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_definition_overlay_scrolls_and_copies() {
    let (mut dash, root) = dashboard("definition");
    open_editor_on(&mut dash, "coder");
    dash.handle_key(key(KeyCode::Char('v')));
    assert!(matches!(
        dash.agents().editor.as_ref().unwrap().overlay,
        Some(Overlay::Definition { scroll: 0 })
    ));
    let screen = text(&mut dash);
    assert!(screen.contains("Definition"), "{screen}");
    assert!(screen.contains("name = \"coder\""), "{screen}");
    for code in [
        KeyCode::Down,
        KeyCode::PageDown,
        KeyCode::Up,
        KeyCode::PageUp,
        KeyCode::End,
        KeyCode::Home,
        KeyCode::Char('j'),
        KeyCode::Char('k'),
        KeyCode::Char('z'),
    ] {
        dash.handle_key(key(code));
    }
    assert!(matches!(
        dash.agents().editor.as_ref().unwrap().overlay,
        Some(Overlay::Definition { scroll: 0 })
    ));
    dash.handle_key(key(KeyCode::End));
    let screen = text(&mut dash);
    assert!(
        !screen.contains("name = \"coder\""),
        "scrolled to the end: {screen}"
    );
    dash.handle_key(key(KeyCode::Char('y')));
    assert!(text(&mut dash).contains("Could not reach a clipboard"));
    dash.yank_fn = |_| true;
    dash.handle_key(key(KeyCode::Char('y')));
    assert!(text(&mut dash).contains("Copied the definition"));
    dash.handle_key(key(KeyCode::Char('v')));
    assert!(dash.agents().editor.as_ref().unwrap().overlay.is_none());
    // Help from the editor names its sections; F1 too.
    dash.handle_key(key(KeyCode::F(1)));
    assert!(text(&mut dash).contains("Agent editor: canvas"));
    dash.handle_key(key(KeyCode::Esc));
    // A narrow terminal shows one pane at a time.
    let narrow = rendered_buffer(&draw(&mut dash, 100, 40));
    assert!(narrow.contains("Graph ·"), "{narrow}");
    assert!(!narrow.contains("This agent"), "{narrow}");
    dash.handle_key(key(KeyCode::Tab));
    let narrow = rendered_buffer(&draw(&mut dash, 100, 40));
    assert!(narrow.contains("This agent"), "{narrow}");
    assert!(!narrow.contains("Graph ·"), "{narrow}");
    // A worker blueprint's node has a panel that says where to edit it.
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .set_stage_mode("plan", &crate::blueprint_edit::StageModeView::FanOut)
        .unwrap();
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .set_fan_out(
            "plan",
            crate::blueprint_edit::FanOutField::Worker(Some((
                crate::blueprint_edit::WorkerKind::Agent,
                "researcher".into(),
            ))),
        )
        .unwrap();
    dash.agents().editor.as_mut().unwrap().refresh();
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .view
        .select_stage("ext:researcher");
    dash.agents().editor.as_mut().unwrap().sync_panel();
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::External("researcher".into())
    );
    let screen = text(&mut dash);
    assert!(screen.contains("Worker blueprint · researcher"), "{screen}");
    assert!(screen.contains("is a separate agent"), "{screen}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_mouse_selects_connects_and_moves_on_the_editor_canvas() {
    let (mut dash, root) = dashboard("mouse");
    open_editor_on(&mut dash, "own");
    draw(&mut dash, 160, 50);
    // A click on the finish box selects it and the panel follows.
    let (x, y, _, _) = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .view
        .node_rect("finish")
        .expect("drawn");
    let (x, y) = (x as u16 + 2, y as u16 + 1);
    dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), x, y));
    dash.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), x, y));
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::Stage {
            name: "finish".into(),
            tab: StageTab::Behaviour
        }
    );
    // Drag it: the arrangement is remembered in the layout store.
    dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), x, y));
    dash.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), x + 6, y + 3));
    dash.handle_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        x + 12,
        y + 6,
    ));
    dash.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), x + 12, y + 6));
    let positions = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .layout
        .positions("own")
        .cloned();
    assert!(
        positions.is_some_and(|p| p.contains_key("finish")),
        "moved and remembered"
    );
    // Drag from finish's source handle onto work: a new path.
    draw(&mut dash, 160, 50);
    let (fx, fy, fr, fb) = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .view
        .node_rect("finish")
        .unwrap();
    let (wx, wy, _, _) = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .view
        .node_rect("work")
        .unwrap();
    let (_, _, _, wb) = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .view
        .node_rect("work")
        .unwrap();
    // The source handle sits on the right border, the target on the left.
    let handle = ((fr - 1) as u16, ((fy + fb) / 2) as u16);
    let target = (wx as u16, ((wy + wb) / 2) as u16);
    let _ = fx;
    dash.handle_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        handle.0,
        handle.1,
    ));
    dash.handle_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        target.0 + 4,
        target.1,
    ));
    dash.handle_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        target.0,
        target.1,
    ));
    dash.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        target.0,
        target.1,
    ));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .edge("finish", "work")
            .is_some(),
        "connected by drag"
    );
    // A wheel over the canvas zooms; a chooser open takes the mouse.
    dash.handle_mouse(mouse(MouseEventKind::ScrollDown, x, y));
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().focus,
        Focus::Inspector,
        "a new path opens its panel"
    );
    dash.handle_key(key(KeyCode::Esc));
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .view
        .select_stage("finish");
    dash.agents().editor.as_mut().unwrap().sync_panel();
    dash.handle_key(key(KeyCode::Char('c')));
    assert!(dash.agents().editor.as_ref().unwrap().picker.is_some());
    dash.handle_mouse(mouse(MouseEventKind::ScrollDown, 5, 5));
    assert!(dash.agents().editor.as_ref().unwrap().picker.is_some());
    dash.handle_key(key(KeyCode::Esc));
    // The catalog preview pans too, and a click elsewhere is a text
    // selection as everywhere.
    dash.handle_key(key(KeyCode::Esc));
    dash.handle_key(key(KeyCode::Char('y')));
    assert!(dash.agent_builder.as_ref().unwrap().editor.is_none());
    draw(&mut dash, 160, 50);
    let preview = dash
        .pane_rects
        .iter()
        .find(|(id, _)| *id == PaneId::AgentsPreview)
        .map(|(_, r)| *r)
        .expect("preview registered");
    dash.handle_mouse(mouse(
        MouseEventKind::ScrollDown,
        preview.x + 5,
        preview.y + 3,
    ));
    dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 2, 3));
    assert!(dash.selection.is_some() || dash.mouse_capture.is_none());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn opening_targets_and_edge_cases_of_the_editor() {
    let (mut dash, root) = dashboard("targets");
    dash.handle_key(key(KeyCode::Char('a')));
    // A manifest the editor cannot hold is a toast.
    dash.open_editor(
        EditTarget::New {
            name: "x".into(),
            bundled_from: None,
        },
        "not = [toml",
    );
    assert!(dash.agents().editor.is_none());
    assert!(
        dash.toasts
            .iter()
            .any(|t| t.message.contains("Cannot edit that manifest"))
    );
    // An existing agent with no directory saves under the agents dir.
    dash.open_editor(
        EditTarget::Existing {
            name: "fresh".into(),
            dir: None,
            bundled_from: None,
        },
        &templates::empty_blueprint("fresh").unwrap(),
    );
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().dir,
        root.join("agents").join("fresh")
    );
    // Ticks animate it; the mouse drain with nothing pending is nothing.
    dash.tick_graphs(std::time::Duration::from_millis(100));
    dash.editor_drain_canvas();
    // Enter on a chooser with an empty filter chooses nothing.
    dash.handle_key(key(KeyCode::Char('c')));
    assert!(
        dash.agents().editor.as_ref().unwrap().picker.is_none(),
        "nothing selected yet"
    );
    dash.handle_key(key(KeyCode::Right));
    dash.handle_key(key(KeyCode::Char('c')));
    type_str(&mut dash, "zzz");
    dash.handle_key(key(KeyCode::Enter));
    assert!(dash.agents().editor.as_ref().unwrap().picker.is_none());
    assert_eq!(dash.agents().editor.as_ref().unwrap().doc.edges().len(), 1);
    // The catalog refresh keeps the cursor on the same agent.
    dash.close_editor();
    let name = dash.agents().catalog.selected_entry().unwrap().name.clone();
    dash.refresh_catalog();
    assert_eq!(dash.agents().catalog.selected_entry().unwrap().name, name);
    let _ = std::fs::remove_dir_all(&root);
}

// ─── the corners ─────────────────────────────────────────────────────────────

#[test]
fn the_corners_of_the_editor() {
    use crate::blueprint_edit::check::{Problem, Problems, Severity};
    let (mut dash, root) = dashboard("corners");
    // A single-stage agent for the catalog's singular, and one whose
    // manifest names no entry stage.
    catalog::write_agent(
        &root.join("agents"),
        "solo",
        "[agent]\nname = \"solo\"\n[stages.only]\nmode = \"autonomous\"\n",
    )
    .unwrap();
    // A clone template whose manifest will not parse is a toast.
    dash.handle_key(key(KeyCode::Char('a')));
    let solo = dash
        .agents()
        .catalog
        .visible()
        .into_iter()
        .position(|i| dash.agents().catalog.entries[i].name == "solo")
        .unwrap();
    dash.agents().catalog.selected = solo;
    let screen = text(&mut dash);
    assert!(screen.contains("solo · v0.1.0 · 1 stage "), "{screen}");
    dash.agents().catalog.entries.push(catalog::CatalogEntry {
        name: "junk".into(),
        version: "0".into(),
        description: String::new(),
        source: catalog::Source::Installed,
        dir: None,
        manifest: Some("not = [".into()),
        stages: vec![],
        bundled: false,
        differs_from_bundled: false,
    });
    dash.handle_key(key(KeyCode::Char('n')));
    let last = dash.agents().chooser.as_ref().unwrap().templates.len() - 1;
    for _ in 0..last {
        dash.handle_key(key(KeyCode::Down));
    }
    dash.handle_key(key(KeyCode::Enter));
    assert!(dash.agents().editor.is_none());
    assert!(
        dash.toasts
            .iter()
            .any(|t| t.message.contains("Could not start from that template"))
    );
    // The editor on solo: no entry stage reads as "(first stage)"; ticking
    // and a confirm drawn over the screen.
    dash.agents().catalog.selected = solo;
    dash.handle_key(key(KeyCode::Enter));
    assert!(text(&mut dash).contains("(first stage)"));
    dash.tick_graphs(std::time::Duration::from_millis(100));
    dash.editor_request_delete_stage("only");
    assert!(dash.pending_confirm.is_none(), "the only stage");
    // Enter and the arrows on a panel with no rows (a worker blueprint's).
    dash.agents().editor.as_mut().unwrap().panel = Panel::External("x".into());
    dash.agents().editor.as_mut().unwrap().focus = Focus::Inspector;
    dash.editor_activate();
    dash.editor_adjust(1);
    dash.handle_key(key(KeyCode::Char('2')));
    dash.handle_key(key(KeyCode::Char('z')));
    assert_eq!(dash.agents().editor.as_ref().unwrap().panel_edge(), None);
    // The arms only tests reach: a toggle, number, choice, button or line
    // committed for a field of another kind does nothing.
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .view
        .select_stage("only");
    dash.agents().editor.as_mut().unwrap().sync_panel();
    dash.editor_set_toggle(&FieldId::StageName, true);
    dash.editor_set_number(&FieldId::StageName, Some(1));
    assert_eq!(
        dash.editor_choice_options(&FieldId::StageName),
        (Vec::new(), None)
    );
    dash.editor_open_choice(&FieldId::StageName);
    assert!(dash.agents().editor.as_ref().unwrap().picker.is_none());
    dash.editor_pick(&FieldId::StageName, "x");
    dash.editor_button(&FieldId::StageName);
    dash.editor_commit_line(&FieldId::StageMode, "x");
    // A choice with nothing to offer: the default model when the catalog is
    // empty.
    dash.agents().editor.as_mut().unwrap().models.clear();
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .view
        .clear_selection();
    dash.agents().editor.as_mut().unwrap().sync_panel();
    dash.agents().editor.as_mut().unwrap().cursor = 2;
    dash.handle_key(key(KeyCode::Right));
    dash.handle_key(key(KeyCode::Enter));
    assert!(dash.agents().editor.as_ref().unwrap().picker.is_none());
    // Failed mutations through the helpers.
    dash.editor_delete_stage("ghost");
    dash.editor_delete_edge("only", "ghost");
    dash.editor_connect("only", "ghost");
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .message
            .as_deref()
            .unwrap()
            .contains("ghost")
    );
    // Renaming to a taken name is refused with the reason.
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .add_stage("other", None)
        .unwrap();
    dash.agents().editor.as_mut().unwrap().refresh();
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .view
        .select_stage("only");
    dash.agents().editor.as_mut().unwrap().sync_panel();
    dash.agents().editor.as_mut().unwrap().focus = Focus::Inspector;
    dash.agents().editor.as_mut().unwrap().cursor = 0;
    dash.handle_key(key(KeyCode::Enter));
    for _ in 0..4 {
        dash.handle_key(key(KeyCode::Backspace));
    }
    type_str(&mut dash, "other");
    let screen = text(&mut dash);
    assert!(
        screen.contains("Name"),
        "the line editor is on screen: {screen}"
    );
    dash.handle_key(key(KeyCode::Enter));
    assert!(text(&mut dash).contains("already taken"));
    // Esc while typing a field drops the edit.
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "zz");
    dash.handle_key(key(KeyCode::Esc));
    assert!(dash.agents().editor.as_ref().unwrap().doc.has_stage("only"));
    // The merge-stage chooser opens; a max-iterations path is labelled.
    let fields = dash.agents().editor.as_ref().unwrap().fields();
    let at = |id: FieldId| fields.iter().position(|f| f.id == id).unwrap();
    dash.agents().editor.as_mut().unwrap().cursor = at(FieldId::StageMode);
    dash.handle_key(key(KeyCode::Right));
    dash.handle_key(key(KeyCode::Right));
    dash.agents().editor.as_mut().unwrap().cursor = at(FieldId::MergeStage);
    dash.handle_key(key(KeyCode::Enter));
    assert!(text(&mut dash).contains("Merge stage"));
    dash.handle_key(key(KeyCode::Esc));
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .add_edge("only", "other")
        .unwrap();
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .set_edge_kind(
            "only",
            "other",
            crate::blueprint_edit::EdgeKind::MaxIterations,
        )
        .unwrap();
    dash.agents().editor.as_mut().unwrap().refresh();
    assert!(text(&mut dash).contains("[too many tries]"));
    // Toggle rows and typing render; problems in every shape render.
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .set_allow_complete("only", Some(true))
        .unwrap();
    dash.agents().editor.as_mut().unwrap().refresh();
    dash.agents().editor.as_mut().unwrap().cursor = at(FieldId::StageDescription);
    dash.handle_key(key(KeyCode::Enter));
    let screen = text(&mut dash);
    assert!(screen.contains("[x] on"), "{screen}");
    dash.handle_key(key(KeyCode::Esc));
    dash.agents().editor.as_mut().unwrap().problems = Problems::default();
    assert!(text(&mut dash).contains("✓ no problems"));
    dash.agents().editor.as_mut().unwrap().problems = Problems {
        items: vec![
            Problem {
                severity: Severity::Error,
                code: "e",
                stage: None,
                message: "broken".into(),
                fix: None,
            },
            Problem {
                severity: Severity::Note,
                code: "n",
                stage: Some("only".into()),
                message: "noted".into(),
                fix: Some("do".into()),
            },
        ],
    };
    dash.agents().editor.as_mut().unwrap().problems_open = true;
    let screen = text(&mut dash);
    assert!(
        screen.contains("1 error") && screen.contains("noted") && screen.contains("(do)"),
        "{screen}"
    );
    // Two errors pluralise the save message.
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .set_stage_mode("only", &crate::blueprint_edit::StageModeView::Autonomous)
        .unwrap();
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .set_tools("only", &["nope1".into()])
        .unwrap();
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .set_tools("other", &["nope2".into()])
        .unwrap();
    dash.handle_key(ctrl('s'));
    let screen = text(&mut dash);
    assert!(screen.contains("2 problems to fix"), "{screen}");
    // A hundred and one edits: the undo stack stays at a hundred.
    for i in 0..101 {
        dash.agents()
            .editor
            .as_mut()
            .unwrap()
            .mutate(|d| {
                d.set_description(&format!("d{i}"));
                Ok(())
            })
            .unwrap();
    }
    assert_eq!(dash.agents().editor.as_ref().unwrap().undo.len(), 100);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_dashboard_without_a_layout_path_keeps_arrangements_in_memory() {
    let mut dash = make_test_dashboard();
    let root =
        std::env::temp_dir().join(format!("lev-agents-screen-memory-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("agents")).unwrap();
    dash.new_run_ctx.agents_dir = root.join("agents");
    dash.new_run_ctx.workdir = root.clone();
    dash.new_run_ctx.config_path = root.join("config.toml");
    catalog::write_agent(
        &root.join("agents"),
        "own",
        &templates::empty_blueprint("own").unwrap(),
    )
    .unwrap();
    open_editor_on(&mut dash, "own");
    assert_eq!(dash.agents().editor.as_ref().unwrap().layout.path(), None);
    dash.handle_key(ctrl('s'));
    assert!(!root.join("dash").exists());
    // Delete from the catalog forgets the arrangement in memory too.
    dash.close_editor();
    dash.perform_agent_delete("own");
    // The inspector for a stage or path that is gone has no rows.
    let doc = crate::blueprint_edit::ManifestDoc::parse(&templates::empty_blueprint("x").unwrap())
        .unwrap();
    assert!(
        super::inspector::fields(
            &doc,
            &Panel::Stage {
                name: "ghost".into(),
                tab: StageTab::Behaviour
            }
        )
        .is_empty()
    );
    assert!(
        super::inspector::fields(
            &doc,
            &Panel::Edge {
                from: "work".into(),
                to: "ghost".into()
            }
        )
        .is_empty()
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn typing_in_a_chooser_never_reaches_the_editor_keys() {
    // `u` is undo on the editor; inside a chooser it is a letter.
    let (mut dash, root) = dashboard("chooser_letters");
    open_editor_on(&mut dash, "coder");
    dash.handle_key(key(KeyCode::Right));
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::Char('2')));
    dash.handle_key(key(KeyCode::Enter));
    assert!(dash.agents().editor.as_ref().unwrap().picker.is_some());
    type_str(&mut dash, "haiku");
    assert!(dash.agents().editor.as_ref().unwrap().picker.is_some());
    assert_eq!(dash.agents().editor.as_ref().unwrap().message, None);
    let _ = std::fs::remove_dir_all(&root);
}
