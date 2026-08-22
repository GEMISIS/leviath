//! The inspector's other panels and the prompts overlay, driven by keys and
//! clicks: a stage's model chain and tools, its context layout and routing,
//! a region, a path's transform, a loop back to the same stage, and the
//! prompts with their round trip through `$EDITOR`.

use crossterm::event::{KeyCode, MouseButton, MouseEventKind};

use super::editor::{Focus, Overlay};
use super::inspector::{FieldId, FieldValue, Panel, StageTab};
use super::prompts::{ExternalEdit, PromptFocus};
use super::tests::{ctrl, dashboard, draw, key, mouse, open_editor_on, text, type_str};
use crate::blueprint_edit::{RegionScope, TransformKind};
use crate::commands::dashboard::state::Dashboard;
use crate::commands::dashboard::test_support::rendered_buffer;

/// Put the inspector cursor on the row with `id` (the row must exist).
fn goto(dash: &mut Dashboard, id: FieldId) {
    let editor = dash.agents().editor.as_mut().unwrap();
    let at = editor
        .fields()
        .iter()
        .position(|f| f.id == id)
        .unwrap_or_else(|| panic!("no row {id:?} on {:?}", editor.panel));
    editor.cursor = at;
    editor.focus = Focus::Inspector;
}

/// Open the editor on `agent`, select `stage` and land on its `tab`.
fn open_stage(dash: &mut Dashboard, agent: &str, stage: &str, tab: StageTab) {
    open_editor_on(dash, agent);
    let editor = dash.agents().editor.as_mut().unwrap();
    editor.view.select_stage(stage);
    editor.sync_panel();
    editor.panel = Panel::Stage {
        name: stage.to_string(),
        tab,
    };
    editor.cursor = 0;
    editor.focus = Focus::Inspector;
}

fn models_of(dash: &mut Dashboard, stage: &str) -> Vec<String> {
    dash.agents()
        .editor
        .as_ref()
        .unwrap()
        .doc
        .stage(stage)
        .unwrap()
        .models
}

fn picker_open(dash: &mut Dashboard) -> bool {
    dash.agents().editor.as_ref().unwrap().picker.is_some()
}

// ─── model & tools ───────────────────────────────────────────────────────────

#[test]
fn the_model_tab_builds_a_chain_and_picks_tools() {
    let (mut dash, root) = dashboard("model_tab");
    open_stage(&mut dash, "own", "work", StageTab::Model);
    // No model yet: one row says so, and Enter on it opens the chooser.
    let fields = dash.agents().editor.as_ref().unwrap().fields();
    assert_eq!(fields[0].id, FieldId::AddModel);
    assert!(matches!(&fields[0].value, FieldValue::Row(r) if r.contains("not set")));
    let screen = text(&mut dash);
    assert!(screen.contains("not set"), "{screen}");
    // ←/→ on that row do nothing (it is not a chain entry).
    dash.handle_key(key(KeyCode::Right));
    assert!(models_of(&mut dash, "work").is_empty());
    dash.handle_key(key(KeyCode::Enter));
    assert!(picker_open(&mut dash));
    let screen = text(&mut dash);
    assert!(screen.contains("Which model?"), "{screen}");
    assert!(screen.contains("context"), "{screen}");
    // Letters go to the search, never to the editor's own keys.
    type_str(&mut dash, "haiku");
    assert!(picker_open(&mut dash));
    dash.handle_key(key(KeyCode::Enter));
    let chain = models_of(&mut dash, "work");
    assert_eq!(chain.len(), 1);
    assert!(chain[0].contains("haiku"), "{chain:?}");
    // Add a fallback, then replace the first entry.
    goto(&mut dash, FieldId::AddModel);
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "sonnet-5");
    dash.handle_key(key(KeyCode::Enter));
    let chain = models_of(&mut dash, "work");
    assert_eq!(chain.len(), 2, "{chain:?}");
    assert!(chain[1].contains("sonnet-5"), "{chain:?}");
    goto(&mut dash, FieldId::ModelEntry(0));
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "opus-5");
    dash.handle_key(key(KeyCode::Enter));
    let chain = models_of(&mut dash, "work");
    assert!(chain[0].contains("opus-5"), "{chain:?}");
    // Move the second one first with ←, and back with →; the cursor follows.
    goto(&mut dash, FieldId::ModelEntry(1));
    dash.handle_key(key(KeyCode::Left));
    let chain = models_of(&mut dash, "work");
    assert!(chain[0].contains("sonnet-5"), "{chain:?}");
    assert_eq!(dash.agents().editor.as_ref().unwrap().cursor, 0);
    dash.handle_key(key(KeyCode::Right));
    let chain = models_of(&mut dash, "work");
    assert!(chain[1].contains("sonnet-5"), "{chain:?}");
    // Off the ends nothing moves.
    dash.handle_key(key(KeyCode::Right));
    assert_eq!(models_of(&mut dash, "work"), chain);
    goto(&mut dash, FieldId::ModelEntry(0));
    dash.handle_key(key(KeyCode::Left));
    assert_eq!(models_of(&mut dash, "work"), chain);
    // Esc in the chooser leaves the chain alone; a replace on a stale index
    // appends instead of panicking.
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::Esc));
    assert_eq!(models_of(&mut dash, "work"), chain);
    dash.editor_settle_more(super::editor::PickerFor::ReplaceModel(9), "x/y");
    assert_eq!(models_of(&mut dash, "work").len(), 3);
    // x drops entries until the row reads "not set" again.
    goto(&mut dash, FieldId::ModelEntry(2));
    dash.handle_key(key(KeyCode::Char('x')));
    goto(&mut dash, FieldId::ModelEntry(1));
    dash.handle_key(key(KeyCode::Delete));
    goto(&mut dash, FieldId::ModelEntry(0));
    dash.handle_key(key(KeyCode::Char('x')));
    assert!(models_of(&mut dash, "work").is_empty());
    // x on a row that is not removable does nothing.
    goto(&mut dash, FieldId::ToolSet);
    dash.handle_key(key(KeyCode::Char('x')));
    // Tools: a multi-chooser; Space toggles, Enter keeps.
    dash.handle_key(key(KeyCode::Enter));
    assert!(picker_open(&mut dash));
    let screen = text(&mut dash);
    assert!(screen.contains("Tools work may use"), "{screen}");
    dash.handle_key(key(KeyCode::Char(' ')));
    dash.handle_key(key(KeyCode::Down));
    dash.handle_key(key(KeyCode::Char(' ')));
    dash.handle_key(key(KeyCode::Enter));
    let tools = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .doc
        .stage("work")
        .unwrap()
        .tools;
    assert_eq!(tools.len(), 2, "{tools:?}");
    let screen = text(&mut dash);
    assert!(screen.contains(&tools[0]), "{screen}");
    // Reopen: the chosen ones are preselected; clearing them removes the key.
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::Char(' ')));
    dash.handle_key(key(KeyCode::Down));
    dash.handle_key(key(KeyCode::Char(' ')));
    dash.handle_key(key(KeyCode::Enter));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work")
            .unwrap()
            .tools
            .is_empty()
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn the_model_chooser_grows_when_the_providers_answer() {
    let (mut dash, root) = dashboard("models_arrive");
    // Opening the screen asks the providers off the loop; with none
    // configured the answer is empty and arrives at once.
    dash.handle_key(key(KeyCode::Char('a')));
    assert!(dash.agents().models_rx.is_some());
    // The answer lands when the task is done, however long the providers
    // take to say no; the channel closing behind it is what says so.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while dash.agents().models_rx.is_some() {
        assert!(
            std::time::Instant::now() < deadline,
            "the providers never answered"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        dash.drain_agents_models();
    }
    // A catalog that names models feeds the editor's chooser when one is
    // open, and the chooser at open when one is not.
    dash.agents().model_catalog = vec!["zeta/live-model".to_string()];
    open_editor_on(&mut dash, "own");
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .models
            .contains(&"zeta/live-model".to_string())
    );
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    dash.agents().models_rx = Some(rx);
    tx.send(vec!["zeta/later-model".to_string()]).unwrap();
    dash.drain_agents_models();
    let models = dash.agents().editor.as_ref().unwrap().models.clone();
    assert!(
        models.contains(&"zeta/later-model".to_string()),
        "{models:?}"
    );
    // The chooser marks what came from the providers and what the agent
    // already names.
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .set_models("work", &["zeta/later-model".to_string()])
        .unwrap();
    dash.agents().editor.as_mut().unwrap().refresh();
    dash.agents().editor.as_mut().unwrap().panel = Panel::Stage {
        name: "work".into(),
        tab: StageTab::Model,
    };
    goto(&mut dash, FieldId::AddModel);
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "zeta/later");
    let screen = text(&mut dash);
    assert!(screen.contains("your provider lists it"), "{screen}");
    assert!(screen.contains("already in this agent"), "{screen}");
    // Draining with no channel, or no screen, is a no-op; a sender still
    // alive but silent leaves the channel in place.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<String>>();
    dash.agents().models_rx = Some(rx);
    dash.drain_agents_models();
    assert!(dash.agents().models_rx.is_some());
    drop(tx);
    dash.drain_agents_models();
    assert!(dash.agents().models_rx.is_none());
    dash.drain_agents_models();
    dash.agent_builder = None;
    dash.drain_agents_models();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn context_windows_read_as_k_or_m() {
    assert_eq!(super::editor_panels::window_label(200_000), "200k");
    assert_eq!(super::editor_panels::window_label(1_000_000), "1M");
    assert_eq!(super::editor_panels::window_label(1_500_000), "1.5M");
}

// ─── context ─────────────────────────────────────────────────────────────────

#[test]
fn the_context_tab_owns_a_layout_adds_regions_and_routes_tools() {
    let (mut dash, root) = dashboard("context_tab");
    open_stage(&mut dash, "own", "work", StageTab::Context);
    let screen = text(&mut dash);
    assert!(screen.contains("shared with the agent"), "{screen}");
    // Adding a region is off while the layout is inherited.
    goto(&mut dash, FieldId::AddRegion);
    assert!(
        !dash
            .agents()
            .editor
            .as_ref()
            .unwrap()
            .current_field()
            .unwrap()
            .enabled
    );
    dash.handle_key(key(KeyCode::Enter));
    assert!(dash.agents().editor.as_ref().unwrap().add_region.is_none());
    // Give the stage its own layout.
    goto(&mut dash, FieldId::OwnLayout);
    dash.handle_key(key(KeyCode::Enter));
    assert!(
        !dash
            .agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .effective_regions(Some("work"))
            .inherited
    );
    let screen = text(&mut dash);
    assert!(screen.contains("its own layout"), "{screen}");
    assert!(screen.contains("own context"), "{screen}");
    // Add a region: the popup, Esc cancels, an empty name is ignored, a
    // name adds it and opens its panel.
    goto(&mut dash, FieldId::AddRegion);
    dash.handle_key(key(KeyCode::Enter));
    assert!(dash.agents().editor.as_ref().unwrap().add_region.is_some());
    let screen = text(&mut dash);
    assert!(screen.contains("New region"), "{screen}");
    dash.handle_key(key(KeyCode::Esc));
    assert!(dash.agents().editor.as_ref().unwrap().add_region.is_none());
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::Enter));
    assert!(dash.agents().editor.as_ref().unwrap().add_region.is_none());
    assert!(matches!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::Stage { .. }
    ));
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "notes");
    dash.handle_key(key(KeyCode::Enter));
    assert!(matches!(
        &dash.agents().editor.as_ref().unwrap().panel,
        Panel::Region { scope: RegionScope::Stage(s), name, .. } if s == "work" && name == "notes"
    ));
    let screen = text(&mut dash);
    assert!(screen.contains("work's own layout"), "{screen}");
    // A bad name is refused with a message and no panel change.
    dash.handle_key(key(KeyCode::Esc));
    goto(&mut dash, FieldId::AddRegion);
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "notes");
    dash.handle_key(key(KeyCode::Enter));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .message
            .as_deref()
            .is_some_and(|m| m.contains("notes"))
    );
    // The region row opens the panel too; Esc comes back to the tab.
    goto(&mut dash, FieldId::StageRegionRow("notes".into()));
    dash.handle_key(key(KeyCode::Enter));
    assert!(matches!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::Region { .. }
    ));
    dash.handle_key(key(KeyCode::Esc));
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::Stage {
            name: "work".into(),
            tab: StageTab::Context
        }
    );
    // Routing: no tools yet, so "route a tool" says so.
    goto(&mut dash, FieldId::AddRouting);
    dash.handle_key(key(KeyCode::Enter));
    assert!(!picker_open(&mut dash));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .message
            .as_deref()
            .is_some_and(|m| m.contains("give it a tool first"))
    );
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .set_tools("work", &["bash".to_string(), "read_file".to_string()])
        .unwrap();
    dash.agents().editor.as_mut().unwrap().refresh();
    // The default region cycles with ←/→ through the stage's regions and
    // the ones every stage has.
    goto(&mut dash, FieldId::RoutingDefault);
    dash.handle_key(key(KeyCode::Right));
    let routing = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .doc
        .tool_routing("work");
    assert_eq!(routing.default_region.as_deref(), Some("notes"));
    dash.handle_key(key(KeyCode::Left));
    let routing = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .doc
        .tool_routing("work");
    assert_eq!(routing.default_region, None);
    // Enter opens a chooser for it.
    dash.handle_key(key(KeyCode::Enter));
    assert!(picker_open(&mut dash));
    type_str(&mut dash, "conversation");
    dash.handle_key(key(KeyCode::Enter));
    let routing = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .doc
        .tool_routing("work");
    assert_eq!(routing.default_region.as_deref(), Some("conversation"));
    // Route one tool: tool chooser, then region chooser.
    goto(&mut dash, FieldId::AddRouting);
    dash.handle_key(key(KeyCode::Enter));
    let screen = text(&mut dash);
    assert!(screen.contains("Route which tool?"), "{screen}");
    type_str(&mut dash, "bash");
    dash.handle_key(key(KeyCode::Enter));
    let screen = text(&mut dash);
    assert!(screen.contains("bash's results land in"), "{screen}");
    type_str(&mut dash, "notes");
    dash.handle_key(key(KeyCode::Enter));
    let routing = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .doc
        .tool_routing("work");
    assert_eq!(
        routing.overrides,
        vec![("bash".to_string(), "notes".to_string())]
    );
    let screen = text(&mut dash);
    assert!(screen.contains("bash → notes"), "{screen}");
    // The routed tool is not offered again; Enter on its row changes the
    // region; x stops routing it.
    goto(&mut dash, FieldId::AddRouting);
    dash.handle_key(key(KeyCode::Enter));
    let offered: Vec<String> = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .picker
        .as_ref()
        .unwrap()
        .1
        .options
        .iter()
        .map(|o| o.value.clone())
        .collect();
    assert_eq!(offered, vec!["read_file".to_string()]);
    dash.handle_key(key(KeyCode::Esc));
    goto(&mut dash, FieldId::RoutingRow("bash".into()));
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "tool_results");
    dash.handle_key(key(KeyCode::Enter));
    let routing = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .doc
        .tool_routing("work");
    assert_eq!(routing.overrides[0].1, "tool_results");
    goto(&mut dash, FieldId::RoutingRow("bash".into()));
    dash.handle_key(key(KeyCode::Char('x')));
    let routing = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .doc
        .tool_routing("work");
    assert!(routing.overrides.is_empty());
    // Back to the shared layout asks first; No keeps it, Yes drops it.
    goto(&mut dash, FieldId::OwnLayout);
    dash.handle_key(key(KeyCode::Enter));
    assert!(dash.pending_confirm.is_some());
    dash.handle_key(key(KeyCode::Char('n')));
    assert!(
        !dash
            .agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .effective_regions(Some("work"))
            .inherited
    );
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::Char('y')));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .effective_regions(Some("work"))
            .inherited
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_agent_panel_opens_a_shared_region_and_adds_one() {
    let (mut dash, root) = dashboard("agent_regions");
    open_editor_on(&mut dash, "coder");
    dash.handle_key(key(KeyCode::Tab));
    assert_eq!(dash.agents().editor.as_ref().unwrap().panel, Panel::Agent);
    let first = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .doc
        .regions(None)
        .first()
        .map(|r| r.name.clone())
        .expect("coder has shared regions");
    goto(&mut dash, FieldId::RegionRow(first.clone()));
    dash.handle_key(key(KeyCode::Enter));
    assert!(matches!(
        &dash.agents().editor.as_ref().unwrap().panel,
        Panel::Region { scope: RegionScope::Shared, name, .. } if *name == first
    ));
    let screen = text(&mut dash);
    assert!(screen.contains("shared layout"), "{screen}");
    // A region added from the agent panel lands in the shared layout.
    dash.handle_key(key(KeyCode::Esc));
    dash.editor_add_region("fresh");
    assert!(matches!(
        &dash.agents().editor.as_ref().unwrap().panel,
        Panel::Region { scope: RegionScope::Shared, name, .. } if name == "fresh"
    ));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .region(None, "fresh")
            .is_some()
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ─── a region ────────────────────────────────────────────────────────────────

#[test]
fn the_region_panel_edits_every_field_and_deletes() {
    let (mut dash, root) = dashboard("region_panel");
    open_stage(&mut dash, "own", "work", StageTab::Context);
    goto(&mut dash, FieldId::OwnLayout);
    dash.handle_key(key(KeyCode::Enter));
    dash.editor_add_region("notes");
    let region = |dash: &mut Dashboard| {
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .region(Some("work"), "notes")
            .unwrap()
    };
    // Kind: ←/→ cycle, Enter opens a chooser with the help line per kind.
    goto(&mut dash, FieldId::RegionKind);
    dash.handle_key(key(KeyCode::Right));
    assert_eq!(region(&mut dash).kind, "temporary");
    dash.handle_key(key(KeyCode::Left));
    assert_eq!(region(&mut dash).kind, "pinned");
    dash.handle_key(key(KeyCode::Enter));
    assert!(picker_open(&mut dash));
    let screen = text(&mut dash);
    assert!(screen.contains("Keeps only the newest items"), "{screen}");
    type_str(&mut dash, "sliding");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(region(&mut dash).kind, "sliding_window");
    // The sliding-window knobs are live now.
    goto(&mut dash, FieldId::RegionMaxItems);
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .current_field()
            .unwrap()
            .enabled
    );
    dash.handle_key(key(KeyCode::Right));
    assert_eq!(region(&mut dash).max_items, Some(1));
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "2");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(region(&mut dash).max_items, Some(12));
    goto(&mut dash, FieldId::RegionStrategy);
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "oldest");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(region(&mut dash).strategy, "oldest");
    goto(&mut dash, FieldId::RegionOverflow);
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "3");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(region(&mut dash).overflow, Some(3));
    // Budget and cap: typed and stepped; a word is refused with a message.
    goto(&mut dash, FieldId::RegionBudget);
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::Backspace));
    type_str(&mut dash, "20");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(region(&mut dash).budget_percent, Some(20.0));
    dash.handle_key(key(KeyCode::Left));
    assert_eq!(region(&mut dash).budget_percent, Some(19.0));
    // A new region carries no ceiling, so the field starts empty and the first
    // step writes one rather than nudging a starter value.
    goto(&mut dash, FieldId::RegionMaxTokens);
    assert_eq!(region(&mut dash).max_tokens, None);
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "4000");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(region(&mut dash).max_tokens, Some(4000));
    dash.handle_key(key(KeyCode::Right));
    assert_eq!(region(&mut dash).max_tokens, Some(4001));
    dash.handle_key(key(KeyCode::Left));
    dash.handle_key(key(KeyCode::Enter));
    for _ in 0..4 {
        dash.handle_key(key(KeyCode::Backspace));
    }
    type_str(&mut dash, "lots");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(region(&mut dash).max_tokens, Some(4000));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .message
            .as_deref()
            .is_some_and(|m| m.contains("whole number"))
    );
    // The floor is the field that matters for a small pinned region, and it
    // edits the same way.
    goto(&mut dash, FieldId::RegionMinTokens);
    assert_eq!(region(&mut dash).min_tokens, None);
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "800");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(region(&mut dash).min_tokens, Some(800));
    dash.handle_key(key(KeyCode::Right));
    assert_eq!(region(&mut dash).min_tokens, Some(801));
    // Back out to the region list, which renders each row's size from whatever
    // the region carries - the percentage, and either absolute when set, which
    // now that the percentage decides is the exception.
    dash.handle_key(key(KeyCode::Esc));
    let listed = text(&mut dash);
    assert!(listed.contains("notes  sliding_window"), "{listed}");
    let row = region(&mut dash);
    assert_eq!((row.min_tokens, row.max_tokens), (Some(801), Some(4000)));
    goto(&mut dash, FieldId::StageRegionRow("notes".into()));
    dash.handle_key(key(KeyCode::Enter));
    // Required: off → on enables the reminder; typing it; off again.
    goto(&mut dash, FieldId::RegionMessage);
    assert!(
        !dash
            .agents()
            .editor
            .as_ref()
            .unwrap()
            .current_field()
            .unwrap()
            .enabled
    );
    goto(&mut dash, FieldId::RegionRequired);
    dash.handle_key(key(KeyCode::Enter));
    assert!(region(&mut dash).required);
    goto(&mut dash, FieldId::RegionMessage);
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .current_field()
            .unwrap()
            .enabled
    );
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "Fill me");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(region(&mut dash).required_message, "Fill me");
    goto(&mut dash, FieldId::RegionRequired);
    dash.handle_key(key(KeyCode::Left));
    assert!(!region(&mut dash).required);
    // Seed and description.
    goto(&mut dash, FieldId::RegionSeed);
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "task");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(region(&mut dash).seed, "task");
    goto(&mut dash, FieldId::RegionDescription);
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "Working notes");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(region(&mut dash).description, "Working notes");
    // Rename: the panel follows the new name; a taken name is refused.
    goto(&mut dash, FieldId::RegionName);
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "2");
    dash.handle_key(key(KeyCode::Enter));
    assert!(matches!(
        &dash.agents().editor.as_ref().unwrap().panel,
        Panel::Region { name, .. } if name == "notes2"
    ));
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, " two");
    dash.handle_key(key(KeyCode::Enter));
    assert!(matches!(
        &dash.agents().editor.as_ref().unwrap().panel,
        Panel::Region { name, .. } if name == "notes2"
    ));
    assert!(dash.agents().editor.as_ref().unwrap().message.is_some());
    // The panel survives a refresh while its region exists.
    dash.agents().editor.as_mut().unwrap().refresh();
    assert!(matches!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::Region { .. }
    ));
    // A second region of the stage's own, opened from its row.
    dash.handle_key(key(KeyCode::Esc));
    goto(&mut dash, FieldId::AddRegion);
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "other");
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::Esc));
    // Delete: the dialog names routing that goes with it; Yes removes it
    // and returns to the tab the panel was opened from.
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .set_tools("work", &["bash".to_string()])
        .unwrap();
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .set_tool_routing_override("work", "bash", "other")
        .unwrap();
    dash.agents().editor.as_mut().unwrap().refresh();
    goto(&mut dash, FieldId::StageRegionRow("other".into()));
    dash.handle_key(key(KeyCode::Enter));
    goto(&mut dash, FieldId::DeleteRegion);
    dash.handle_key(key(KeyCode::Enter));
    assert!(dash.pending_confirm.is_some());
    let screen = text(&mut dash);
    assert!(screen.contains("land in it"), "{screen}");
    dash.handle_key(key(KeyCode::Char('y')));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .region(Some("work"), "other")
            .is_none()
    );
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::Stage {
            name: "work".into(),
            tab: StageTab::Context
        }
    );
    // A region panel whose region is gone underneath it has no rows.
    assert!(
        super::inspector::fields(
            &dash.agents().editor.as_ref().unwrap().doc,
            &Panel::Region {
                scope: RegionScope::Stage("work".into()),
                name: "ghost".into(),
                back: Box::new(Panel::Agent),
            }
        )
        .is_empty()
    );
    // The region-scoped helpers are inert off a region panel.
    dash.editor_set_toggle_more(&FieldId::RegionRequired, true);
    assert!(!dash.editor_set_number_more(&FieldId::StageName, None));
    dash.editor_pick_more(&FieldId::RegionKind, "pinned");
    dash.editor_commit_line_more(&FieldId::RegionName, "x");
    dash.editor_commit_line_more(&FieldId::RegionSeed, "x");
    dash.editor_button_more(&FieldId::DeleteRegion);
    assert!(dash.pending_confirm.is_none());
    let _ = std::fs::remove_dir_all(&root);
}

// ─── a path's transform, and a loop ──────────────────────────────────────────

#[test]
fn the_path_panel_sets_the_transform_its_rules_and_the_summary_prompt() {
    let (mut dash, root) = dashboard("path_transform");
    open_editor_on(&mut dash, "coder");
    // Pick the first path the coder has.
    let edge = dash.agents().editor.as_ref().unwrap().doc.edges()[0].clone();
    let editor = dash.agents().editor.as_mut().unwrap();
    editor.view.select_edge(&edge.from, &edge.to);
    editor.sync_panel();
    editor.focus = Focus::Inspector;
    assert!(matches!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::Edge { .. }
    ));
    let transform = |dash: &mut Dashboard| {
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .edge(&edge.from, &edge.to)
            .unwrap()
            .transform
    };
    // → cycles through every kind and wraps; the per-region rows only
    // wake up on custom.
    goto(&mut dash, FieldId::EdgeTransform);
    let start = transform(&mut dash);
    let mut seen = vec![start.clone()];
    for _ in 0..TransformKind::CHOICES.len() {
        dash.handle_key(key(KeyCode::Right));
        seen.push(transform(&mut dash));
    }
    assert_eq!(seen.last(), Some(&start), "{seen:?}");
    assert!(seen.contains(&TransformKind::Custom), "{seen:?}");
    dash.handle_key(key(KeyCode::Enter));
    assert!(picker_open(&mut dash));
    type_str(&mut dash, "custom");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(transform(&mut dash), TransformKind::Custom);
    let fields = dash.agents().editor.as_ref().unwrap().fields();
    let rule = fields
        .iter()
        .find(|f| matches!(f.id, FieldId::TransformRule(_)) && f.enabled)
        .cloned()
        .expect("a non-pinned region has a live rule row");
    let FieldId::TransformRule(region) = rule.id.clone() else {
        unreachable!()
    };
    let rules = |dash: &mut Dashboard| {
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .edge(&edge.from, &edge.to)
            .unwrap()
            .rules
    };
    goto(&mut dash, rule.id.clone());
    // Enter, → and ← all step the segment; the screen shows the bracketed
    // choice.
    dash.handle_key(key(KeyCode::Right));
    let after_right = rules(&mut dash);
    dash.handle_key(key(KeyCode::Enter));
    let after_enter = rules(&mut dash);
    assert_ne!(after_right, after_enter);
    dash.handle_key(key(KeyCode::Left));
    assert_eq!(rules(&mut dash), after_right);
    let screen = text(&mut dash);
    assert!(screen.contains('['), "{screen}");
    // A region in the clear list, then stepping from it.
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .set_transform_rule(
            &edge.from,
            &edge.to,
            &region,
            crate::blueprint_edit::Rule::Clear,
        )
        .unwrap();
    dash.agents().editor.as_mut().unwrap().refresh();
    goto(&mut dash, rule.id.clone());
    dash.handle_key(key(KeyCode::Right));
    assert!(!rules(&mut dash).clear.contains(&region));
    // The summary prompt: a typed line.
    goto(&mut dash, FieldId::CompactPrompt);
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "Keep the decisions");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(rules(&mut dash).compact_prompt, "Keep the decisions");
    // Segment cycling on something that is not a rule row is a no-op.
    dash.editor_cycle_segment(&FieldId::EdgeHint, 1);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_loop_back_to_the_same_stage_has_its_own_path_panel() {
    let (mut dash, root) = dashboard("self_loop");
    open_stage(&mut dash, "own", "work", StageTab::Behaviour);
    // No loop yet: no row for it.
    assert!(
        !dash
            .agents()
            .editor
            .as_ref()
            .unwrap()
            .fields()
            .iter()
            .any(|f| f.id == FieldId::SelfLoop)
    );
    // `c` → itself opens the loop's panel straight away.
    dash.agents().editor.as_mut().unwrap().focus = Focus::Canvas;
    dash.handle_key(key(KeyCode::Char('c')));
    type_str(&mut dash, "itself");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::Edge {
            from: "work".into(),
            to: "work".into()
        }
    );
    let screen = text(&mut dash);
    assert!(screen.contains("back to itself"), "{screen}");
    // The panel stays across refreshes while the loop exists.
    dash.agents().editor.as_mut().unwrap().refresh();
    assert!(matches!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::Edge { .. }
    ));
    // Esc returns to the stage; the behaviour tab now lists the loop, and
    // Enter on that row reopens the panel.
    dash.handle_key(key(KeyCode::Esc));
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::Stage {
            name: "work".into(),
            tab: StageTab::Behaviour
        }
    );
    goto(&mut dash, FieldId::SelfLoop);
    let screen = text(&mut dash);
    assert!(screen.contains("Loops back to itself"), "{screen}");
    dash.handle_key(key(KeyCode::Enter));
    assert!(matches!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::Edge { .. }
    ));
    // Deleting the loop from its panel drops back to what the canvas shows.
    goto(&mut dash, FieldId::DeletePath);
    dash.handle_key(key(KeyCode::Enter));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .edge("work", "work")
            .is_none()
    );
    assert!(matches!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::Stage { .. }
    ));
    // Leaving from a stage panel with no anchor is the plain Esc: canvas.
    dash.handle_key(key(KeyCode::Esc));
    assert_eq!(dash.agents().editor.as_ref().unwrap().focus, Focus::Canvas);
    // A leave with a stale anchor and a plain panel changes nothing.
    dash.editor_leave_region();
    let _ = std::fs::remove_dir_all(&root);
}

// ─── the prompts overlay and $EDITOR ─────────────────────────────────────────

#[test]
fn the_prompts_overlay_edits_applies_and_discards() {
    let (mut dash, root) = dashboard("prompts_overlay");
    open_stage(&mut dash, "own", "work", StageTab::Behaviour);
    goto(&mut dash, FieldId::EditPrompts);
    dash.handle_key(key(KeyCode::Enter));
    assert!(matches!(
        dash.agents().editor.as_ref().unwrap().overlay,
        Some(Overlay::Prompts(_))
    ));
    let screen = text(&mut dash);
    assert!(screen.contains("Prompts · work"), "{screen}");
    assert!(screen.contains("System prompt"), "{screen}");
    assert!(screen.contains("Transition prompt"), "{screen}");
    assert!(screen.contains("F2 $EDITOR"), "{screen}");
    // Typing lands in the focused box; Tab moves to the other; the keys on
    // the editor underneath (v, u, p) are letters here.
    type_str(&mut dash, " vup");
    dash.handle_key(key(KeyCode::Tab));
    type_str(&mut dash, "Go to finish.");
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "Second line.");
    let prompts = match &dash.agents().editor.as_ref().unwrap().overlay {
        Some(Overlay::Prompts(p)) => (*p).clone(),
        _ => unreachable!(),
    };
    assert_eq!(prompts.focus, PromptFocus::Transition);
    assert!(prompts.system.lines().join("\n").ends_with(" vup"));
    dash.handle_key(key(KeyCode::BackTab));
    assert!(matches!(
        &dash.agents().editor.as_ref().unwrap().overlay,
        Some(Overlay::Prompts(p)) if p.focus == PromptFocus::System
    ));
    // Ctrl-S applies both and closes; a multi-line prompt ends in a newline.
    dash.handle_key(ctrl('s'));
    assert!(dash.agents().editor.as_ref().unwrap().overlay.is_none());
    let stage = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .doc
        .stage("work")
        .unwrap();
    assert!(
        stage.system_prompt.ends_with(" vup"),
        "{:?}",
        stage.system_prompt
    );
    assert_eq!(stage.transition_prompt, "Go to finish.\nSecond line.\n");
    // Ctrl-Q discards.
    goto(&mut dash, FieldId::EditPrompts);
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "lost");
    dash.handle_key(ctrl('q'));
    assert!(dash.agents().editor.as_ref().unwrap().overlay.is_none());
    let stage = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .doc
        .stage("work")
        .unwrap();
    assert!(!stage.system_prompt.contains("lost"));
    // Esc applies too.
    goto(&mut dash, FieldId::EditPrompts);
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, " kept");
    dash.handle_key(key(KeyCode::Esc));
    let stage = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .doc
        .stage("work")
        .unwrap();
    assert!(
        stage.system_prompt.ends_with(" kept"),
        "{:?}",
        stage.system_prompt
    );
    // Opening prompts on a stage that is gone is a no-op.
    dash.agents().editor.as_mut().unwrap().panel = Panel::Stage {
        name: "ghost".into(),
        tab: StageTab::Behaviour,
    };
    dash.editor_open_prompts();
    assert!(dash.agents().editor.as_ref().unwrap().overlay.is_none());
    // The prompt keys do nothing with no overlay open.
    dash.editor_prompts_key(&ctrl('s'));
    let _ = std::fs::remove_dir_all(&root);
}

/// The prompts overlay is two long-form boxes side by side, so it is where a
/// toolbar press has to pick the right one. Clicking the transition box's `B`
/// formats *that* box and moves the keys to it.
#[test]
fn a_prompt_boxs_toolbar_formats_the_box_that_was_clicked() {
    let (mut dash, root) = dashboard("prompts_toolbar");
    open_stage(&mut dash, "own", "work", StageTab::Behaviour);
    goto(&mut dash, FieldId::EditPrompts);
    dash.handle_key(key(KeyCode::Enter));
    assert!(matches!(
        &dash.agents().editor.as_ref().unwrap().overlay,
        Some(Overlay::Prompts(p)) if p.focus == PromptFocus::System
    ));

    // Both boxes draw a toolbar; the second one down belongs to the
    // transition prompt.
    let buttons = bold_buttons(&mut dash, 160, 50);
    assert_eq!(buttons.len(), 2, "one toolbar per prompt box");
    let (x, y) = buttons[1];
    dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), x, y));
    let editor = dash.agents().editor.as_ref().unwrap();
    assert!(matches!(
        &editor.overlay,
        Some(Overlay::Prompts(p))
            if p.focus == PromptFocus::Transition && p.transition.text() == "****"
    ));

    // A press on the system box's toolbar goes back to it.
    let (x, y) = buttons[0];
    let _ = draw(&mut dash, 160, 50);
    dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), x, y));
    assert!(matches!(
        &dash.agents().editor.as_ref().unwrap().overlay,
        Some(Overlay::Prompts(p)) if p.focus == PromptFocus::System
    ));

    // Away from either toolbar, nothing formats.
    assert!(!dash.prompts_toolbar_click(x, y + 3));

    // Nor does it with the overlay closed, with the editor closed, or with no
    // agents screen at all: the overlay's boxes are the only thing it owns.
    dash.handle_key(ctrl('q'));
    assert!(!dash.prompts_toolbar_click(x, y));
    dash.agents().editor = None;
    assert!(!dash.prompts_toolbar_click(x, y));
    dash.agent_builder = None;
    assert!(!dash.prompts_toolbar_click(x, y));
    let _ = std::fs::remove_dir_all(&root);
}

/// Every `B` button drawn in the frame, top to bottom. The row reads
/// `" B │ I │ ..."`, so a `B` with an `I` four columns on is a toolbar.
fn bold_buttons(dash: &mut Dashboard, w: u16, h: u16) -> Vec<(u16, u16)> {
    let terminal = draw(dash, w, h);
    let buf = terminal.backend().buffer().clone();
    let at = |x: u16, y: u16| buf.cell((x, y)).map(|c| c.symbol().to_string());
    let mut found = Vec::new();
    for y in 0..h {
        for x in 0..w.saturating_sub(4) {
            if at(x, y).as_deref() == Some("B") && at(x + 4, y).as_deref() == Some("I") {
                found.push((x, y));
            }
        }
    }
    found
}

/// The chord path through the same overlay. `Ctrl-E` is inline code here, the
/// same as in every other long-form box: this is the overlay that used to
/// spend that chord on `$EDITOR`, which is now on `F2`.
#[test]
fn formatting_chords_reach_the_focused_prompt_box() {
    let (mut dash, root) = dashboard("prompts_chords");
    dash.external_edit_dir = root.join("scratch");
    open_stage(&mut dash, "own", "work", StageTab::Behaviour);
    goto(&mut dash, FieldId::EditPrompts);
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::Tab));

    dash.handle_key(ctrl('b'));
    type_str(&mut dash, "loud");
    dash.handle_key(ctrl('e'));
    type_str(&mut dash, "code");
    assert!(matches!(
        &dash.agents().editor.as_ref().unwrap().overlay,
        Some(Overlay::Prompts(p)) if p.transition.text() == "**loud`code`**"
    ));
    // And it formatted rather than reaching for an editor.
    assert!(!dash.has_external_edit());

    // F1 opens the help without typing into the prompt.
    dash.handle_key(key(KeyCode::F(1)));
    assert!(dash.show_help);
    assert!(matches!(
        &dash.agents().editor.as_ref().unwrap().overlay,
        Some(Overlay::Prompts(p)) if p.transition.text() == "**loud`code`**"
    ));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn f2_hands_a_prompt_to_the_editor_and_takes_it_back() {
    let (mut dash, root) = dashboard("prompts_external");
    dash.external_edit_dir = root.join("scratch");
    open_stage(&mut dash, "own", "work", StageTab::Behaviour);
    goto(&mut dash, FieldId::EditPrompts);
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::Tab));
    type_str(&mut dash, "Before.");
    assert!(!dash.has_external_edit());
    dash.handle_key(key(KeyCode::F(2)));
    assert!(dash.has_external_edit());
    let edit = dash.take_external_edit().expect("a file to open");
    assert!(!dash.has_external_edit());
    assert_eq!(edit.target, PromptFocus::Transition);
    assert_eq!(std::fs::read_to_string(&edit.path).unwrap(), "Before.");
    // The "editor" rewrites the file; the text comes back into the box and
    // the file is gone.
    std::fs::write(&edit.path, "After.\n").unwrap();
    let path = edit.path.clone();
    dash.finish_external_edit(edit, Ok(()));
    assert!(!path.exists());
    assert!(matches!(
        &dash.agents().editor.as_ref().unwrap().overlay,
        Some(Overlay::Prompts(p)) if p.transition.lines() == ["After."]
    ));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .message
            .as_deref()
            .is_some_and(|m| m.contains("updated"))
    );
    // An editor that failed leaves the box alone and says so.
    dash.handle_key(key(KeyCode::F(2)));
    let edit = dash.take_external_edit().unwrap();
    dash.finish_external_edit(edit, Err(std::io::Error::other("no editor")));
    assert!(matches!(
        &dash.agents().editor.as_ref().unwrap().overlay,
        Some(Overlay::Prompts(p)) if p.transition.lines() == ["After."]
    ));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .message
            .as_deref()
            .is_some_and(|m| m.contains("no editor"))
    );
    // A file that vanished reads the same way.
    dash.handle_key(key(KeyCode::F(2)));
    let edit = dash.take_external_edit().unwrap();
    std::fs::remove_file(&edit.path).unwrap();
    dash.finish_external_edit(edit, Ok(()));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .message
            .as_deref()
            .is_some_and(|m| m.contains("did not hand"))
    );
    // Coming back with the overlay closed, or the editor closed, or the
    // screen closed, drops the text quietly.
    dash.handle_key(key(KeyCode::F(2)));
    let edit = dash.take_external_edit().unwrap();
    dash.handle_key(ctrl('q'));
    dash.finish_external_edit(edit.clone(), Ok(()));
    dash.handle_key(key(KeyCode::Esc));
    dash.handle_key(key(KeyCode::Esc));
    dash.handle_key(key(KeyCode::Char('y')));
    assert!(dash.agents().editor.is_none());
    dash.finish_external_edit(edit.clone(), Ok(()));
    dash.agent_builder = None;
    dash.finish_external_edit(edit, Ok(()));
    // A scratch directory that cannot be made: a message, nothing pending.
    std::fs::write(root.join("blocked"), "not a dir").unwrap();
    dash.external_edit_dir = root.join("blocked");
    open_stage(&mut dash, "own", "work", StageTab::Behaviour);
    goto(&mut dash, FieldId::EditPrompts);
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::F(2)));
    assert!(!dash.has_external_edit());
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .message
            .as_deref()
            .is_some_and(|m| m.contains("Could not hand"))
    );
    // F2 with no overlay is a no-op.
    dash.handle_key(ctrl('q'));
    dash.editor_request_external_edit();
    assert!(!dash.has_external_edit());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_external_edit_shape_is_plain_data() {
    let edit = ExternalEdit {
        path: "/tmp/x".into(),
        target: PromptFocus::System,
    };
    assert_eq!(edit.clone(), edit);
    assert!(format!("{edit:?}").contains("System"));
}

// ─── the mouse on the inspector ──────────────────────────────────────────────

#[test]
fn a_click_on_the_inspector_picks_rows_and_tabs() {
    let (mut dash, root) = dashboard("inspector_mouse");
    open_stage(&mut dash, "own", "work", StageTab::Behaviour);
    let _ = draw(&mut dash, 160, 50);
    let hit = dash.agents().editor.as_ref().unwrap().hit.clone();
    assert!(!hit.rows.is_empty());
    let (tab_row, tabs) = hit.tabs.clone().expect("a stage panel has tabs");
    // A click on the third tab switches to it.
    let press = |col: u16, row: u16| mouse(MouseEventKind::Down(MouseButton::Left), col, row);
    assert!(dash.handle_agents_mouse(press(tabs[2].0 + 1, tab_row)));
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::Stage {
            name: "work".into(),
            tab: StageTab::Context
        }
    );
    let _ = draw(&mut dash, 160, 50);
    let hit = dash.agents().editor.as_ref().unwrap().hit.clone();
    // A click on a row moves the cursor there; a second click opens it
    // (the own-layout button acts).
    let own = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .fields()
        .iter()
        .position(|f| f.id == FieldId::OwnLayout)
        .unwrap();
    dash.agents().editor.as_mut().unwrap().focus = Focus::Canvas;
    assert!(dash.handle_agents_mouse(press(hit.area.x + 4, hit.rows[own])));
    assert_eq!(dash.agents().editor.as_ref().unwrap().cursor, own);
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().focus,
        Focus::Inspector
    );
    assert!(dash.handle_agents_mouse(press(hit.area.x + 4, hit.rows[own])));
    assert!(
        !dash
            .agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .effective_regions(Some("work"))
            .inherited
    );
    // A click on the inspector's empty space only takes the focus; outside
    // it, on the canvas, the click is the canvas's; other buttons and
    // drags are not clicks; a line being typed keeps the mouse out.
    dash.agents().editor.as_mut().unwrap().focus = Focus::Canvas;
    assert!(dash.handle_agents_mouse(press(hit.area.x + 4, hit.area.y + hit.area.height - 2)));
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().focus,
        Focus::Inspector
    );
    assert!(!dash.editor_inspector_mouse(press(2, hit.rows[own])));
    assert!(!dash.editor_inspector_mouse(mouse(
        MouseEventKind::Down(MouseButton::Right),
        hit.area.x + 4,
        hit.rows[own]
    )));
    goto(&mut dash, FieldId::RoutingDefault);
    dash.handle_key(key(KeyCode::Enter));
    assert!(picker_open(&mut dash));
    dash.handle_key(key(KeyCode::Esc));
    dash.agents().editor.as_mut().unwrap().line = Some((
        FieldId::StageName,
        crate::tui::widgets::line_edit::LineEdit::new(String::new(), false),
    ));
    assert!(!dash.editor_inspector_mouse(press(hit.area.x + 4, hit.rows[own])));
    dash.agents().editor.as_mut().unwrap().line = None;
    // A tab click on a panel without tabs is a row click.
    dash.handle_key(key(KeyCode::Tab));
    dash.handle_key(key(KeyCode::Tab));
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .view
        .clear_selection();
    dash.agents().editor.as_mut().unwrap().sync_panel();
    let _ = draw(&mut dash, 160, 50);
    let hit = dash.agents().editor.as_ref().unwrap().hit.clone();
    assert!(hit.tabs.is_none());
    assert!(dash.handle_agents_mouse(press(hit.area.x + 4, hit.rows[0])));
    assert_eq!(dash.agents().editor.as_ref().unwrap().cursor, 0);
    // No editor: not handled.
    dash.close_editor();
    assert!(!dash.editor_inspector_mouse(press(1, 1)));
    let _ = std::fs::remove_dir_all(&root);
}

// ─── drawing ─────────────────────────────────────────────────────────────────

#[test]
fn segments_buttons_and_name_popups_draw() {
    let (mut dash, root) = dashboard("panel_drawing");
    open_stage(&mut dash, "own", "work", StageTab::Context);
    goto(&mut dash, FieldId::OwnLayout);
    dash.handle_key(key(KeyCode::Enter));
    dash.editor_add_region("scratch");
    dash.handle_key(key(KeyCode::Esc));
    // A button row is its label, arrowed.
    let screen = text(&mut dash);
    assert!(screen.contains("▸ Add a region"), "{screen}");
    assert!(screen.contains("▸ Back to the shared layout"), "{screen}");
    // The add-stage popup and the add-region popup share a frame.
    dash.agents().editor.as_mut().unwrap().focus = Focus::Canvas;
    dash.handle_key(key(KeyCode::Char('a')));
    let screen = text(&mut dash);
    assert!(screen.contains("New stage"), "{screen}");
    assert!(screen.contains("enter apply"), "{screen}");
    dash.handle_key(key(KeyCode::Esc));
    // A path with custom rules draws the segment with the choice bracketed.
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .set_transform("work", "finish", &TransformKind::Custom)
        .unwrap();
    dash.agents().editor.as_mut().unwrap().refresh();
    let editor = dash.agents().editor.as_mut().unwrap();
    editor.view.select_edge("work", "finish");
    editor.sync_panel();
    editor.focus = Focus::Inspector;
    let screen = text(&mut dash);
    assert!(screen.contains("Per-region rules"), "{screen}");
    assert!(
        screen.contains("[carry]") || screen.contains("[Carry]") || screen.contains("carried"),
        "{screen}"
    );
    // The overlay's hint bar and the picker's hint bar.
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_prompts_overlay_draws_on_a_small_terminal_too() {
    let (mut dash, root) = dashboard("prompts_small");
    open_stage(&mut dash, "own", "work", StageTab::Behaviour);
    goto(&mut dash, FieldId::EditPrompts);
    dash.handle_key(key(KeyCode::Enter));
    let screen = rendered_buffer(&draw(&mut dash, 80, 20));
    assert!(screen.contains("System prompt"), "{screen}");
    assert!(screen.contains("tab other prompt"), "{screen}");
    let _ = std::fs::remove_dir_all(&root);
}

// ─── the corners ─────────────────────────────────────────────────────────────

#[test]
fn a_bundled_agent_opened_for_editing_brings_its_scripts_and_takes_them_back() {
    let (mut dash, root) = dashboard("bundled_scratch");
    // The data analyst is bundled, not installed, and ships scripts:
    // editing it materialises its directory so the lint can see them.
    open_editor_on(&mut dash, "data-analyst");
    let dir = dash.agents().editor.as_ref().unwrap().dir.clone();
    assert!(dash.agents().editor.as_ref().unwrap().scratch_dir);
    assert!(dir.exists(), "{}", dir.display());
    assert!(!dir.join("agent.leviath").exists());
    // Closing without saving leaves nothing behind.
    dash.close_editor();
    assert!(!dir.exists());
    // Saved, it stays: a complete install.
    open_editor_on(&mut dash, "data-analyst");
    dash.handle_key(ctrl('s'));
    assert!(dir.join("agent.leviath").exists());
    dash.close_editor();
    assert!(dir.join("agent.leviath").exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_helpers_are_inert_off_their_panels_and_rows() {
    let (mut dash, root) = dashboard("panel_corners");
    open_stage(&mut dash, "own", "work", StageTab::Behaviour);
    // ←/→ on a text row or a button change nothing.
    goto(&mut dash, FieldId::StageDescription);
    dash.handle_key(key(KeyCode::Right));
    goto(&mut dash, FieldId::EditPrompts);
    dash.handle_key(key(KeyCode::Left));
    assert!(dash.agents().editor.as_ref().unwrap().overlay.is_none());
    // Enter on a plain status row opens nothing.
    dash.handle_key(key(KeyCode::Char('3')));
    goto(&mut dash, FieldId::ContextStatus);
    dash.handle_key(key(KeyCode::Enter));
    assert!(!picker_open(&mut dash));
    // The region-scoped number helper answers "mine" for a region field
    // even off a region panel, and does nothing.
    assert!(dash.editor_set_number_more(&FieldId::RegionBudget, None));
    dash.set_region_panel_name("x");
    // A settle for a chooser purpose that is not a pick is a no-op.
    dash.editor_settle_more(super::editor::PickerFor::Tools, "x");
    dash.editor_settle_more(super::editor::PickerFor::Field(FieldId::StageMode), "x");
    // x with no row under the cursor.
    dash.agents().editor.as_mut().unwrap().panel = Panel::Stage {
        name: "ghost".into(),
        tab: StageTab::Model,
    };
    dash.editor_remove_row();
    // A delete-region off a region panel still deletes, with nowhere to
    // go back to; a cycle on a path that is gone does nothing.
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .add_region(&RegionScope::Shared, "loose")
        .unwrap();
    dash.agents().editor.as_mut().unwrap().refresh();
    dash.agents().editor.as_mut().unwrap().panel = Panel::Agent;
    dash.editor_delete_region(&RegionScope::Shared, "loose");
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .region(None, "loose")
            .is_none()
    );
    dash.agents().editor.as_mut().unwrap().panel = Panel::Edge {
        from: "work".into(),
        to: "ghost".into(),
    };
    dash.editor_cycle_segment(&FieldId::TransformRule("x".into()), 1);
    // A rule row for a region in no list steps to the first rule going
    // forward and the last going back.
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .set_transform("work", "finish", &TransformKind::Custom)
        .unwrap();
    dash.agents().editor.as_mut().unwrap().refresh();
    let editor = dash.agents().editor.as_mut().unwrap();
    editor.view.select_edge("work", "finish");
    editor.sync_panel();
    dash.editor_cycle_segment(&FieldId::TransformRule("nowhere".into()), -1);
    dash.editor_cycle_segment(&FieldId::TransformRule("elsewhere".into()), 1);
    let rules = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .doc
        .edge("work", "finish")
        .unwrap()
        .rules;
    assert!(rules.clear.contains(&"nowhere".to_string()), "{rules:?}");
    assert!(rules.carry.contains(&"elsewhere".to_string()), "{rules:?}");
    // Applying prompts with no overlay open does nothing.
    dash.editor_apply_prompts();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_stage_that_inherits_lists_the_shared_regions_and_a_table_seed_reads_as_such() {
    let (mut dash, root) = dashboard("coder_context");
    let stage = {
        let (mut d, _) = (dashboard("coder_context_probe").0, ());
        open_editor_on(&mut d, "coder");
        d.agents().editor.as_ref().unwrap().doc.stage_names()[0].clone()
    };
    open_stage(&mut dash, "coder", &stage, StageTab::Context);
    let screen = text(&mut dash);
    assert!(screen.contains("shared with the agent"), "{screen}");
    assert!(screen.contains("  shared"), "{screen}");
    // A shared region row opened from the stage's tab is the shared region.
    let first = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .doc
        .regions(None)
        .first()
        .map(|r| r.name.clone())
        .unwrap();
    goto(&mut dash, FieldId::StageRegionRow(first.clone()));
    dash.handle_key(key(KeyCode::Enter));
    assert!(matches!(
        &dash.agents().editor.as_ref().unwrap().panel,
        Panel::Region { scope: RegionScope::Shared, name, .. } if *name == first
    ));
    dash.handle_key(key(KeyCode::Esc));
    // The routing chooser lists the stage's regions once even when one of
    // them is a name every stage has.
    let (options, _) = dash.editor_choice_options_more(&FieldId::RoutingDefault);
    let mut sorted = options.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(options.len(), sorted.len(), "{options:?}");
    assert!(options.contains(&"conversation".to_string()), "{options:?}");
    // A region seeded from files says so, and the seed is not editable.
    dash.handle_key(key(KeyCode::Tab));
    dash.handle_key(key(KeyCode::Tab));
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .view
        .clear_selection();
    dash.agents().editor.as_mut().unwrap().sync_panel();
    goto(&mut dash, FieldId::RegionRow("conventions".into()));
    dash.handle_key(key(KeyCode::Enter));
    goto(&mut dash, FieldId::RegionSeed);
    assert!(
        !dash
            .agents()
            .editor
            .as_ref()
            .unwrap()
            .current_field()
            .unwrap()
            .enabled
    );
    let screen = text(&mut dash);
    assert!(screen.contains("(files, a command"), "{screen}");
    // A region with neither budget nor cap shows neither.
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .set_region_field(
            &RegionScope::Shared,
            "conventions",
            crate::blueprint_edit::RegionField::BudgetPercent,
            crate::blueprint_edit::RegionValue::Number(None),
        )
        .unwrap();
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .set_region_field(
            &RegionScope::Shared,
            "conventions",
            crate::blueprint_edit::RegionField::MaxTokens,
            crate::blueprint_edit::RegionValue::Number(None),
        )
        .unwrap();
    dash.agents().editor.as_mut().unwrap().refresh();
    dash.handle_key(key(KeyCode::Esc));
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .view
        .select_stage(&stage);
    dash.agents().editor.as_mut().unwrap().sync_panel();
    dash.agents().editor.as_mut().unwrap().panel = Panel::Stage {
        name: stage.clone(),
        tab: StageTab::Context,
    };
    let rows = dash.agents().editor.as_ref().unwrap().fields();
    let conventions = rows
        .iter()
        .find(|f| f.id == FieldId::StageRegionRow("conventions".into()))
        .unwrap();
    assert!(
        matches!(&conventions.value, FieldValue::Row(r) if !r.contains('%') && !r.contains("tokens"))
    );
    // Delete a region nothing routes into: the dialog is one line.
    goto(&mut dash, FieldId::StageRegionRow("conventions".into()));
    dash.handle_key(key(KeyCode::Enter));
    goto(&mut dash, FieldId::DeleteRegion);
    dash.handle_key(key(KeyCode::Enter));
    let screen = text(&mut dash);
    assert!(!screen.contains("land in it"), "{screen}");
    dash.handle_key(key(KeyCode::Char('y')));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .region(None, "conventions")
            .is_none()
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_system_prompt_comes_back_from_the_editor_too() {
    let (mut dash, root) = dashboard("prompts_system_back");
    dash.external_edit_dir = root.join("scratch");
    open_stage(&mut dash, "own", "work", StageTab::Behaviour);
    goto(&mut dash, FieldId::EditPrompts);
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::F(2)));
    let edit = dash.take_external_edit().unwrap();
    assert_eq!(edit.target, PromptFocus::System);
    std::fs::write(&edit.path, "Rewritten.").unwrap();
    dash.finish_external_edit(edit, Ok(()));
    assert!(matches!(
        &dash.agents().editor.as_ref().unwrap().overlay,
        Some(Overlay::Prompts(p)) if p.system.lines() == ["Rewritten."]
    ));
    let _ = std::fs::remove_dir_all(&root);
}
