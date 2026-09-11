//! The inspector's other panels and the prompts overlay, driven by keys and
//! clicks: a stage's model chain and tools, its context layout and routing,
//! a region, a path's transform, a loop back to the same stage, and the
//! prompts with their round trip through `$EDITOR`.

use crossterm::event::{KeyCode, MouseButton, MouseEventKind};

use super::editor::{Focus, ModelDrag, Overlay};
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
    dash.handle_key(key(KeyCode::Char('l')));
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
    dash.handle_key(key(KeyCode::Char('h')));
    let chain = models_of(&mut dash, "work");
    assert!(chain[0].contains("sonnet-5"), "{chain:?}");
    assert_eq!(dash.agents().editor.as_ref().unwrap().cursor, 0);
    dash.handle_key(key(KeyCode::Char('l')));
    let chain = models_of(&mut dash, "work");
    assert!(chain[1].contains("sonnet-5"), "{chain:?}");
    // Off the ends nothing moves.
    dash.handle_key(key(KeyCode::Char('l')));
    assert_eq!(models_of(&mut dash, "work"), chain);
    goto(&mut dash, FieldId::ModelEntry(0));
    dash.handle_key(key(KeyCode::Char('h')));
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

/// The MCP servers answer off the loop: the config's from the moment the
/// screen opens, an agent's own from the moment its editor does, and the
/// chooser grows as each lands. A server picked in the chooser is a
/// connector; one of its tools is a tool.
#[test]
fn the_tools_chooser_offers_mcp_servers_and_their_tools_as_they_answer() {
    let (mut dash, root) = dashboard("mcp_tools");
    // The config names one server; the agent's manifest another.
    std::fs::write(
        &dash.new_run_ctx.config_path,
        "[[mcp_servers]]\nname = \"github\"\ncommand = \"true\"\n",
    )
    .unwrap();
    let manifest = root.join("agents").join("own").join("agent.leviath");
    let mut manifest_text = std::fs::read_to_string(&manifest).unwrap();
    manifest_text.push_str("\n[[mcp_servers]]\nname = \"mine\"\ncommand = \"true\"\n");
    std::fs::write(&manifest, manifest_text).unwrap();
    open_stage(&mut dash, "own", "work", StageTab::Model);
    // Without a runtime both are pending, and both are offered already.
    assert_eq!(
        dash.agents().mcp.get("github"),
        Some(&super::McpServerTools::Pending)
    );
    assert_eq!(
        dash.agents().mcp.get("mine"),
        Some(&super::McpServerTools::Pending)
    );
    goto(&mut dash, FieldId::ToolSet);
    dash.handle_key(key(KeyCode::Enter));
    let values = picker_values(&mut dash);
    assert!(values.contains(&"github".to_string()), "{values:?}");
    assert!(values.contains(&"mine".to_string()), "{values:?}");
    dash.handle_key(key(KeyCode::Esc));
    // The answers land: one server lists, one fails.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    dash.agents().mcp_rx = Some(rx);
    tx.send(("github".to_string(), Ok(vec!["search".to_string()])))
        .unwrap();
    tx.send(("mine".to_string(), Err("no such command".to_string())))
        .unwrap();
    dash.drain_agents_models();
    let tools = dash.agents().editor.as_ref().unwrap().tools.clone();
    assert!(
        tools.iter().any(|t| t.name == "github__search"),
        "{tools:?}"
    );
    assert!(
        tools
            .iter()
            .any(|t| t.name == "mine" && t.detail.contains("no such command")),
        "{tools:?}"
    );
    // Picking the server grants it whole; picking its tool grants the tool.
    dash.handle_key(key(KeyCode::Enter));
    picker_goto(&mut dash, "github");
    dash.handle_key(key(KeyCode::Char(' ')));
    picker_goto(&mut dash, "github__search");
    dash.handle_key(key(KeyCode::Char(' ')));
    dash.handle_key(key(KeyCode::Enter));
    let stage = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .doc
        .stage("work")
        .unwrap();
    assert_eq!(stage.connectors, ["github"]);
    assert_eq!(stage.tools, ["github__search"]);
    let row = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .fields()
        .into_iter()
        .find(|f| f.id == FieldId::ToolSet)
        .map(|f| f.value)
        .unwrap();
    assert!(
        matches!(&row, FieldValue::Row(r) if r == "github__search, github (MCP, every tool)"),
        "{row:?}"
    );
    // Reopened, both are picked; dropping the server keeps the tool.
    dash.handle_key(key(KeyCode::Enter));
    {
        let editor = dash.agents().editor.as_ref().unwrap();
        let picker = &editor.picker.as_ref().unwrap().1;
        let at = |v: &str| picker.options.iter().position(|o| o.value == v).unwrap();
        assert!(picker.is_chosen(at("github")));
        assert!(picker.is_chosen(at("github__search")));
    }
    picker_goto(&mut dash, "github");
    dash.handle_key(key(KeyCode::Char(' ')));
    dash.handle_key(key(KeyCode::Enter));
    let stage = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .doc
        .stage("work")
        .unwrap();
    assert!(stage.connectors.is_empty());
    assert_eq!(stage.tools, ["github__search"]);
    // A feed that closes is let go of; asking again about a known server
    // asks nothing.
    drop(tx);
    dash.drain_agents_models();
    assert!(dash.agents().mcp_rx.is_none());
    dash.drain_agents_models();
    dash.ask_mcp_servers(&[leviath_mcp::MCPServerConfig {
        name: "github".to_string(),
        ..Default::default()
    }]);
    assert!(matches!(
        dash.agents().mcp.get("github"),
        Some(super::McpServerTools::Listed(_))
    ));
    let _ = std::fs::remove_dir_all(&root);
}

/// With a runtime under it, opening the screen asks each configured
/// server for its tools off the loop, and the answer lands in the catalog.
#[tokio::test]
async fn the_mcp_servers_are_asked_off_the_loop() {
    let (mut dash, root) = dashboard("mcp_asked");
    std::fs::write(
        &dash.new_run_ctx.config_path,
        "[[mcp_servers]]\nname = \"dead\"\ncommand = \"/nonexistent/mcp-server-binary\"\n",
    )
    .unwrap();
    dash.handle_key(key(KeyCode::Char('a')));
    assert_eq!(
        dash.agents().mcp.get("dead"),
        Some(&super::McpServerTools::Pending)
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while dash.agents().mcp.get("dead") == Some(&super::McpServerTools::Pending) {
        assert!(
            std::time::Instant::now() < deadline,
            "the server never answered"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        dash.drain_agents_models();
    }
    assert!(matches!(
        dash.agents().mcp.get("dead"),
        Some(super::McpServerTools::Failed(_))
    ));
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
    dash.handle_key(key(KeyCode::Char('l')));
    let routing = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .doc
        .tool_routing("work");
    assert_eq!(routing.default_region.as_deref(), Some("notes"));
    dash.handle_key(key(KeyCode::Char('h')));
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

// ─── inputs & outputs, and the tool limits ───────────────────────────────

/// The chooser rows on screen, by their values, while a chooser is open.
fn picker_values(dash: &mut Dashboard) -> Vec<String> {
    dash.agents()
        .editor
        .as_ref()
        .unwrap()
        .picker
        .as_ref()
        .map(|(_, p)| p.options.iter().map(|o| o.value.clone()).collect())
        .unwrap_or_default()
}

/// Move the open chooser's cursor onto `value` (the row must be there).
fn picker_goto(dash: &mut Dashboard, value: &str) {
    let at = picker_values(dash)
        .iter()
        .position(|v| v == value)
        .unwrap_or_else(|| panic!("no chooser row {value}"));
    let editor = dash.agents().editor.as_mut().unwrap();
    let picker = &mut editor.picker.as_mut().unwrap().1;
    picker.query = crate::tui::widgets::line_edit::LineEdit::new(String::new(), false);
    picker.cursor = at;
}

#[test]
fn the_inputs_and_outputs_tab_picks_types_and_opens_files_in_a_window() {
    let (mut dash, root) = dashboard("io_tab");
    // The chooser reads the registry the daemon would: a row of the
    // operator's shows up in it.
    std::fs::write(
        &dash.new_run_ctx.config_path,
        "[mime_types.\"application/x-acme\"]\nfamily = \"model\"\n",
    )
    .unwrap();
    // And a row of the blueprint's own, which its runs see on top.
    let manifest = root.join("agents").join("own").join("agent.leviath");
    let mut manifest_text = std::fs::read_to_string(&manifest).unwrap();
    manifest_text.push_str("\n[mime_types.\"application/x-mine\"]\nfamily = \"model\"\n");
    std::fs::write(&manifest, manifest_text).unwrap();
    open_stage(&mut dash, "own", "work", StageTab::Behaviour);
    dash.handle_key(key(KeyCode::Char('2')));
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::Stage {
            name: "work".into(),
            tab: StageTab::Io
        }
    );
    let screen = text(&mut dash);
    assert!(screen.contains("2 Inputs & outputs"), "{screen}");
    let stage = |dash: &mut Dashboard| {
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work")
            .unwrap()
    };
    // What the stage takes: a chooser of families and registry types,
    // Space picks, Enter keeps.
    goto(&mut dash, FieldId::StageAccepts);
    dash.handle_key(key(KeyCode::Enter));
    assert!(picker_open(&mut dash));
    let values = picker_values(&mut dash);
    assert!(values.contains(&"image/*".to_string()), "{values:?}");
    assert!(
        values.contains(&"application/x-mine".to_string()),
        "{values:?}"
    );
    assert!(values.contains(&"image/png".to_string()), "{values:?}");
    assert!(
        values.contains(&"application/x-acme".to_string()),
        "{values:?}"
    );
    assert_eq!(values.last().map(String::as_str), Some("another…"));
    picker_goto(&mut dash, "image/*");
    dash.handle_key(key(KeyCode::Char(' ')));
    picker_goto(&mut dash, "audio/wav");
    dash.handle_key(key(KeyCode::Char(' ')));
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(stage(&mut dash).input_accepts, ["image/*", "audio/wav"]);
    // Reopened, the chooser has them picked and its cursor on the first.
    dash.handle_key(key(KeyCode::Enter));
    {
        let editor = dash.agents().editor.as_ref().unwrap();
        let picker = &editor.picker.as_ref().unwrap().1;
        assert!(picker.is_chosen(picker.cursor));
    }
    dash.handle_key(key(KeyCode::Esc));
    // "another…" opens the line editor with what was picked already in it.
    goto(&mut dash, FieldId::StageAsText);
    dash.handle_key(key(KeyCode::Enter));
    picker_goto(&mut dash, "model/*");
    dash.handle_key(key(KeyCode::Char(' ')));
    picker_goto(&mut dash, "another…");
    dash.handle_key(key(KeyCode::Char(' ')));
    dash.handle_key(key(KeyCode::Enter));
    assert!(dash.agents().editor.as_ref().unwrap().line.is_some());
    type_str(&mut dash, "application/x-scene");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(
        stage(&mut dash).input_as_text,
        ["model/*", "application/x-scene"]
    );
    // Reopened, a typed type the list never had is a row of its own.
    dash.handle_key(key(KeyCode::Enter));
    assert!(picker_values(&mut dash).contains(&"application/x-scene".to_string()));
    dash.handle_key(key(KeyCode::Esc));
    // x clears a list.
    dash.handle_key(key(KeyCode::Char('x')));
    assert!(stage(&mut dash).input_as_text.is_empty());
    goto(&mut dash, FieldId::StageAccepts);
    dash.handle_key(key(KeyCode::Char('x')));
    assert!(stage(&mut dash).input_accepts.is_empty());
    // The output type is one pick from the plain shapes and the registry;
    // "(any)" and x both ask for no shape, and "another…" types one.
    goto(&mut dash, FieldId::OutputFormat);
    let screen = text(&mut dash);
    assert!(screen.contains("Output type"), "{screen}");
    assert!(screen.contains("(any)"), "{screen}");
    dash.handle_key(key(KeyCode::Enter));
    assert!(picker_open(&mut dash));
    let values = picker_values(&mut dash);
    assert_eq!(&values[..4], ["(any)", "markdown", "json", "text"]);
    assert!(values.contains(&"image/png".to_string()));
    picker_goto(&mut dash, "markdown");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(stage(&mut dash).output_format, "markdown");
    dash.handle_key(key(KeyCode::Enter));
    {
        let editor = dash.agents().editor.as_ref().unwrap();
        let picker = &editor.picker.as_ref().unwrap().1;
        assert_eq!(picker.options[picker.cursor].value, "markdown");
    }
    picker_goto(&mut dash, "(any)");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(stage(&mut dash).output_format, "");
    dash.handle_key(key(KeyCode::Enter));
    picker_goto(&mut dash, "another…");
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "a2ui");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(stage(&mut dash).output_format, "a2ui");
    dash.handle_key(key(KeyCode::Char('x')));
    assert_eq!(stage(&mut dash).output_format, "");
    // Declaring a file asks its name and opens its window.
    goto(&mut dash, FieldId::AddArtifact);
    dash.handle_key(key(KeyCode::Enter));
    let screen = text(&mut dash);
    assert!(screen.contains("New artifact"), "{screen}");
    // Esc on the name prompt declares nothing; an empty name neither.
    dash.handle_key(key(KeyCode::Esc));
    goto(&mut dash, FieldId::AddArtifact);
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::Enter));
    assert!(stage(&mut dash).artifacts.is_empty());
    goto(&mut dash, FieldId::AddArtifact);
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "final");
    dash.handle_key(key(KeyCode::Enter));
    assert!(matches!(
        &dash.agents().editor.as_ref().unwrap().panel,
        Panel::Artifact { stage, index: 0 } if stage == "work"
    ));
    let screen = text(&mut dash);
    assert!(screen.contains("File · work hands back #1"), "{screen}");
    assert!(screen.contains("Esc closes this window"), "{screen}");
    // The type is one pick; "another…" types one in.
    goto(&mut dash, FieldId::ArtifactType);
    dash.handle_key(key(KeyCode::Enter));
    picker_goto(&mut dash, "video/mp4");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(stage(&mut dash).artifacts[0].mime_type, "video/mp4");
    // "another…" with nothing typed leaves the type alone.
    dash.handle_key(key(KeyCode::Enter));
    picker_goto(&mut dash, "another…");
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(stage(&mut dash).artifacts[0].mime_type, "video/mp4");
    dash.handle_key(key(KeyCode::Enter));
    picker_goto(&mut dash, "another…");
    dash.handle_key(key(KeyCode::Enter));
    for _ in 0..10 {
        dash.handle_key(key(KeyCode::Backspace));
    }
    type_str(&mut dash, "video/x-cut");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(stage(&mut dash).artifacts[0].mime_type, "video/x-cut");
    goto(&mut dash, FieldId::ArtifactRequired);
    dash.handle_key(key(KeyCode::Enter));
    goto(&mut dash, FieldId::ArtifactDescription);
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "the cut");
    dash.handle_key(key(KeyCode::Enter));
    goto(&mut dash, FieldId::ArtifactName);
    dash.handle_key(key(KeyCode::Enter));
    for _ in 0..5 {
        dash.handle_key(key(KeyCode::Backspace));
    }
    type_str(&mut dash, "cut");
    dash.handle_key(key(KeyCode::Enter));
    let a = stage(&mut dash).artifacts.remove(0);
    assert_eq!(
        (
            a.name.as_str(),
            a.mime_type.as_str(),
            a.required,
            a.description.as_str()
        ),
        ("cut", "video/x-cut", true, "the cut")
    );
    // A window keeps the keys: Tab moves nothing, a click outside it is
    // swallowed, a click on a row picks it and a second opens it.
    dash.handle_key(key(KeyCode::Tab));
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().focus,
        Focus::Inspector
    );
    let _ = draw(&mut dash, 160, 50);
    let hit = dash.agents().editor.as_ref().unwrap().modal_hit.clone();
    assert!(!hit.rows.is_empty());
    let press = |col: u16, row: u16| mouse(MouseEventKind::Down(MouseButton::Left), col, row);
    assert!(dash.handle_agents_mouse(press(0, 0)));
    assert!(dash.handle_agents_mouse(mouse(
        MouseEventKind::ScrollDown,
        hit.area.x + 2,
        hit.rows[0]
    )));
    let required = 2;
    // A click left of the window is swallowed without moving the cursor.
    assert!(dash.handle_agents_mouse(press(0, hit.rows[required])));
    assert_ne!(dash.agents().editor.as_ref().unwrap().cursor, required);
    assert!(dash.handle_agents_mouse(press(hit.area.x + 2, hit.rows[required])));
    assert_eq!(dash.agents().editor.as_ref().unwrap().cursor, required);
    assert!(dash.handle_agents_mouse(press(hit.area.x + 2, hit.rows[required])));
    assert!(!stage(&mut dash).artifacts[0].required);
    // Esc closes the window; the row it came from is under the cursor.
    dash.handle_key(key(KeyCode::Esc));
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().panel,
        Panel::Stage {
            name: "work".into(),
            tab: StageTab::Io
        }
    );
    let screen = text(&mut dash);
    assert!(screen.contains("cut  video/x-cut"), "{screen}");
    // Enter on the row reopens it; the drop button closes it with the file gone.
    goto(&mut dash, FieldId::ArtifactRow(0));
    dash.handle_key(key(KeyCode::Enter));
    goto(&mut dash, FieldId::DeleteArtifact);
    dash.handle_key(key(KeyCode::Enter));
    assert!(stage(&mut dash).artifacts.is_empty());
    assert!(dash.agents().editor.as_ref().unwrap().modal.is_none());
    // x on a row drops a declaration too; a taken name is refused.
    dash.editor_add_artifact("a");
    dash.handle_key(key(KeyCode::Esc));
    dash.editor_add_artifact("b");
    dash.handle_key(key(KeyCode::Esc));
    dash.editor_add_artifact("a");
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .message
            .as_deref()
            .is_some_and(|m| m.contains("taken"))
    );
    let screen = text(&mut dash);
    assert_eq!(screen.matches("Output file").count(), 2, "{screen}");
    goto(&mut dash, FieldId::ArtifactRow(1));
    dash.handle_key(key(KeyCode::Char('x')));
    goto(&mut dash, FieldId::ArtifactRow(0));
    dash.handle_key(key(KeyCode::Char('x')));
    assert!(stage(&mut dash).artifacts.is_empty());
    // The regions the stage reads open in the window from here too.
    dash.handle_key(key(KeyCode::Char('4')));
    goto(&mut dash, FieldId::OwnLayout);
    dash.handle_key(key(KeyCode::Enter));
    dash.editor_add_region("shots");
    dash.handle_key(key(KeyCode::Esc));
    dash.editor_add_region("notes");
    dash.handle_key(key(KeyCode::Esc));
    dash.handle_key(key(KeyCode::Char('2')));
    let screen = text(&mut dash);
    assert!(
        screen.contains("Input types")
            && screen.contains("from notes")
            && screen.contains("any type"),
        "{screen}"
    );
    goto(&mut dash, FieldId::IoRegionRow("shots".into()));
    dash.handle_key(key(KeyCode::Enter));
    assert!(matches!(
        &dash.agents().editor.as_ref().unwrap().panel,
        Panel::Region { name, .. } if name == "shots"
    ));
    let region = |dash: &mut Dashboard| {
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .region(Some("work"), "shots")
            .unwrap()
    };
    goto(&mut dash, FieldId::RegionAccepts);
    dash.handle_key(key(KeyCode::Enter));
    picker_goto(&mut dash, "image/png");
    dash.handle_key(key(KeyCode::Char(' ')));
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(region(&mut dash).accepts, ["image/png"]);
    goto(&mut dash, FieldId::RegionAccepts);
    dash.handle_key(key(KeyCode::Char('x')));
    assert!(region(&mut dash).accepts.is_empty());
    dash.handle_key(key(KeyCode::Esc));
    let screen = text(&mut dash);
    assert!(screen.contains("shots  any type"), "{screen}");
    // The graph beside the inspector wears the badges as you edit.
    goto(&mut dash, FieldId::StageAccepts);
    dash.handle_key(key(KeyCode::Enter));
    picker_goto(&mut dash, "audio/*");
    dash.handle_key(key(KeyCode::Char(' ')));
    dash.handle_key(key(KeyCode::Enter));
    let screen = text(&mut dash);
    assert!(screen.contains("◧ audio/*"), "{screen}");
    // With nothing of its own, the input row shows what the regions take
    // between them: the union of their lists (text aside), or any type
    // when one of them takes anything.
    goto(&mut dash, FieldId::StageAccepts);
    dash.handle_key(key(KeyCode::Char('x')));
    let set_accepts = |dash: &mut Dashboard, name: &str, list: &str| {
        dash.agents()
            .editor
            .as_mut()
            .unwrap()
            .doc
            .set_region_field(
                &RegionScope::Stage("work".into()),
                name,
                crate::blueprint_edit::RegionField::Accepts,
                crate::blueprint_edit::RegionValue::Text(list.to_string()),
            )
            .unwrap();
        dash.agents().editor.as_mut().unwrap().refresh();
    };
    let input_row = |dash: &mut Dashboard| {
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .fields()
            .into_iter()
            .find(|f| f.id == FieldId::StageAccepts)
            .map(|f| match f.value {
                FieldValue::Row(r) => r,
                other => panic!("{other:?}"),
            })
            .unwrap()
    };
    set_accepts(&mut dash, "shots", "image/png");
    set_accepts(&mut dash, "notes", "text/plain, audio/*");
    assert_eq!(
        input_row(&mut dash),
        "(image/png, audio/*, from its regions)"
    );
    let screen = text(&mut dash);
    assert!(screen.contains("from shots  image/png"), "{screen}");
    set_accepts(&mut dash, "notes", "");
    assert_eq!(input_row(&mut dash), "(any type, from its regions)");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn the_models_tab_limits_what_each_tool_may_be_handed() {
    let (mut dash, root) = dashboard("tool_limits");
    open_stage(&mut dash, "own", "work", StageTab::Model);
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .set_tools(
            "work",
            &[
                "@scripts".to_string(),
                "read_file".to_string(),
                "spawn_agent".to_string(),
            ],
        )
        .unwrap();
    dash.agents().editor.as_mut().unwrap().refresh();
    // A group grant has no row of its own.
    assert!(
        !dash
            .agents()
            .editor
            .as_ref()
            .unwrap()
            .fields()
            .iter()
            .any(|f| f.id == FieldId::ToolLimitRow("@scripts".into()))
    );
    let limits = |dash: &mut Dashboard| {
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .stage("work")
            .unwrap()
            .tool_accepts
    };
    let screen = text(&mut dash);
    assert!(screen.contains("spawn_agent accepts"), "{screen}");
    assert!(screen.contains("any type"), "{screen}");
    goto(&mut dash, FieldId::ToolLimitRow("spawn_agent".into()));
    dash.handle_key(key(KeyCode::Enter));
    let screen = text(&mut dash);
    assert!(
        screen.contains("What spawn_agent may be handed here"),
        "{screen}"
    );
    picker_goto(&mut dash, "image/*");
    dash.handle_key(key(KeyCode::Char(' ')));
    picker_goto(&mut dash, "audio/*");
    dash.handle_key(key(KeyCode::Char(' ')));
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(
        limits(&mut dash),
        vec![(
            "spawn_agent".to_string(),
            vec!["image/*".to_string(), "audio/*".to_string()]
        )]
    );
    let saved = dash.agents().editor.as_ref().unwrap().doc.to_toml();
    assert!(saved.contains("[stages.work.tool_accepts]"), "{saved}");
    // A second tool's limit sits beside the first, and reopening either
    // chooser finds its own.
    goto(&mut dash, FieldId::ToolLimitRow("read_file".into()));
    dash.handle_key(key(KeyCode::Enter));
    picker_goto(&mut dash, "text/*");
    dash.handle_key(key(KeyCode::Char(' ')));
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(limits(&mut dash).len(), 2);
    dash.handle_key(key(KeyCode::Enter));
    {
        let editor = dash.agents().editor.as_ref().unwrap();
        let picker = &editor.picker.as_ref().unwrap().1;
        assert!(picker.is_chosen(picker.cursor));
    }
    dash.handle_key(key(KeyCode::Esc));
    dash.handle_key(key(KeyCode::Char('x')));
    assert_eq!(limits(&mut dash).len(), 1);
    // A limit on a tool the stage no longer names keeps its row, so it can
    // be lifted; x lifts it.
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .set_tools("work", &["read_file".to_string()])
        .unwrap();
    dash.agents().editor.as_mut().unwrap().refresh();
    goto(&mut dash, FieldId::ToolLimitRow("spawn_agent".into()));
    dash.handle_key(key(KeyCode::Char('x')));
    assert!(limits(&mut dash).is_empty());
    assert!(
        !dash
            .agents()
            .editor
            .as_ref()
            .unwrap()
            .fields()
            .iter()
            .any(|f| f.id == FieldId::ToolLimitRow("spawn_agent".into()))
    );
    // The chooser on a tool row lands on the typed path too.
    goto(&mut dash, FieldId::ToolLimitRow("read_file".into()));
    dash.handle_key(key(KeyCode::Enter));
    picker_goto(&mut dash, "another…");
    dash.handle_key(key(KeyCode::Char(' ')));
    dash.handle_key(key(KeyCode::Enter));
    type_str(&mut dash, "text/csv");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(
        limits(&mut dash),
        vec![("read_file".to_string(), vec!["text/csv".to_string()])]
    );
    let _ = std::fs::remove_dir_all(root);
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
            }
        )
        .is_empty()
    );
    // The region-scoped helpers are inert off a region panel, the file
    // ones off a file's window; a type chooser opened off its panel holds
    // nothing yet; a delete that is refused leaves the panel alone.
    dash.editor_set_toggle_more(&FieldId::RegionRequired, true);
    assert!(!dash.editor_set_number_more(&FieldId::StageName, None));
    dash.editor_set_toggle_more(&FieldId::ArtifactRequired, true);
    dash.editor_set_toggle_more(&FieldId::StageMode, true);
    dash.editor_commit_line_more(&FieldId::ArtifactName, "x");
    dash.editor_commit_line_more(&FieldId::ArtifactType, "x/y");
    dash.editor_commit_line_more(&FieldId::RegionAccepts, "image/*");
    dash.editor_button_more(&FieldId::DeleteArtifact);
    dash.editor_delete_region(&RegionScope::Shared, "ghost");
    dash.editor_open_row(&FieldId::ArtifactType);
    assert!(picker_open(&mut dash));
    dash.handle_key(key(KeyCode::Esc));
    dash.editor_open_row(&FieldId::RegionAccepts);
    assert!(picker_open(&mut dash));
    dash.handle_key(key(KeyCode::Esc));
    assert!(
        super::inspector::fields(
            &dash.agents().editor.as_ref().unwrap().doc,
            &Panel::Artifact {
                stage: "work".into(),
                index: 9,
            }
        )
        .is_empty()
    );
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
    // Closing with no window up changes nothing.
    dash.editor_close_modal();
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

    // The pointer moving over a button names it on that box's border.
    let (bx, by) = buttons[0];
    dash.handle_mouse(mouse(MouseEventKind::Moved, bx, by));
    let screen = text(&mut dash);
    assert!(screen.contains("bold"), "the border names it: {screen}");

    // Away from either toolbar, nothing formats: a press in the text moves
    // the caret there instead.
    let before = match &dash.agents().editor.as_ref().unwrap().overlay {
        Some(Overlay::Prompts(p)) => p.system.text(),
        _ => unreachable!(),
    };
    dash.prompts_toolbar_click(x, y + 3);
    let after = match &dash.agents().editor.as_ref().unwrap().overlay {
        Some(Overlay::Prompts(p)) => p.system.text(),
        _ => unreachable!(),
    };
    assert_eq!(before, after, "a press in the text formatted something");

    // The overlay's title row belongs to neither box.
    assert!(!dash.prompts_toolbar_click(0, 0));

    // Nor does it with the overlay closed, with the editor closed, or with no
    // agents screen at all: the overlay's boxes are the only thing it owns.
    dash.handle_key(ctrl('q'));
    assert!(!dash.prompts_toolbar_click(x, y));
    dash.prompts_toolbar_hover(x, y);
    dash.agents().editor = None;
    assert!(!dash.prompts_toolbar_click(x, y));
    dash.prompts_toolbar_hover(x, y);
    dash.agent_builder = None;
    assert!(!dash.prompts_toolbar_click(x, y));
    dash.prompts_toolbar_hover(x, y);
    let _ = std::fs::remove_dir_all(&root);
}

/// Every `B` button drawn in the frame, top to bottom. The row reads
/// `" B  i  S  U "`, so a `B` with an `i` three columns on is a toolbar.
fn bold_buttons(dash: &mut Dashboard, w: u16, h: u16) -> Vec<(u16, u16)> {
    let terminal = draw(dash, w, h);
    let buf = terminal.backend().buffer().clone();
    let at = |x: u16, y: u16| buf.cell((x, y)).map(|c| c.symbol().to_string());
    let mut found = Vec::new();
    for y in 0..h {
        for x in 0..w.saturating_sub(3) {
            if at(x, y).as_deref() == Some("B") && at(x + 3, y).as_deref() == Some("i") {
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

/// A prompt box's popup outranks the overlay's keys: Esc closes the popup,
/// not the whole overlay, and the prompts are not applied behind it.
#[test]
fn a_prompt_boxs_popup_outranks_the_overlays_keys() {
    let (mut dash, root) = dashboard("prompts_popup");
    open_stage(&mut dash, "own", "work", StageTab::Behaviour);
    goto(&mut dash, FieldId::EditPrompts);
    dash.handle_key(key(KeyCode::Enter));

    dash.handle_key(ctrl('k'));
    assert!(matches!(
        &dash.agents().editor.as_ref().unwrap().overlay,
        Some(Overlay::Prompts(p)) if p.system.is_modal()
    ));
    dash.handle_key(key(KeyCode::Esc));
    assert!(
        matches!(
            &dash.agents().editor.as_ref().unwrap().overlay,
            Some(Overlay::Prompts(p)) if !p.system.is_modal()
        ),
        "Esc closed the popup and left the overlay open"
    );
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
    // The scratch directory is made once, under whatever the parent was at
    // the time; a dashboard never changes its parent, so a test that does
    // has to forget the directory it already made.
    dash.external_edit_scratch = None;
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
    let _ = draw(&mut dash, 200, 50);
    let hit = dash.agents().editor.as_ref().unwrap().hit.clone();
    assert!(!hit.rows.is_empty());
    let (tab_row, tabs) = hit.tabs.clone().expect("a stage panel has tabs");
    // The strip fits the inspector on one line, and the rows are drawn
    // where the map says: a wrapped strip once pushed every row down a line,
    // so a click landed on the row below the one under the pointer.
    let screen = text(&mut dash);
    // The buffer is one string of cells, 200 to a row.
    let row_text =
        |row: u16| -> String { screen.chars().skip(row as usize * 200).take(200).collect() };
    assert!(
        row_text(tab_row).contains("4 Context"),
        "{}",
        row_text(tab_row)
    );
    assert!(
        tabs.last()
            .is_some_and(|(_, x1)| *x1 <= hit.area.x + hit.area.width),
        "{tabs:?} within {:?}",
        hit.area
    );
    assert!(
        row_text(hit.rows[0]).contains("Name"),
        "{}",
        row_text(hit.rows[0])
    );
    assert!(
        row_text(hit.rows[1]).contains("How it works"),
        "{}",
        row_text(hit.rows[1])
    );
    // A click on the last tab switches to it.
    let press = |col: u16, row: u16| mouse(MouseEventKind::Down(MouseButton::Left), col, row);
    assert!(dash.handle_agents_mouse(press(tabs[3].0 + 1, tab_row)));
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

/// The model chain is a priority order, so moving an entry is an edit like
/// any other - and reaching for the mouse to do it is the obvious thing.
///
/// The grip is what makes that safe. Dragging the *row* would mean the only
/// way to copy a model id out of the inspector was to not touch the row it is
/// on, so the press has to land on the grip cells to pick anything up.
#[test]
fn a_model_is_dragged_along_the_chain_by_its_grip() {
    let (mut dash, root) = dashboard("model_drag");
    open_stage(&mut dash, "own", "work", StageTab::Model);
    let chain = vec![
        "alfa-model".to_string(),
        "bravo-model".to_string(),
        "charlie-model".to_string(),
    ];
    assert!(dash.editor_mutate(|d| d.set_models("work", &chain)));

    let _ = draw(&mut dash, 160, 50);
    let grips = dash.agents().editor.as_ref().unwrap().hit.grips.clone();
    assert_eq!(
        grips.iter().map(|(m, _)| *m).collect::<Vec<_>>(),
        vec![0, 1, 2],
        "one grip per chain entry, in chain order"
    );
    // Only the model rows get one: the tools row below them is not orderable.
    assert_eq!(
        grips.len(),
        3,
        "the button and the tools row carry no grip: {grips:?}"
    );

    let at = |kind, cell: ratatui::layout::Rect| mouse(kind, cell.x, cell.y);
    let down = MouseEventKind::Down(MouseButton::Left);
    let drag = MouseEventKind::Drag(MouseButton::Left);
    let up = MouseEventKind::Up(MouseButton::Left);

    // Pick the last entry up. Nothing is written yet - the document is only
    // touched on the drop, so the whole gesture is one undo entry.
    assert!(dash.handle_agents_mouse(at(down, grips[2].1)));
    assert_eq!(
        models_of(&mut dash, "work"),
        chain,
        "the press changed nothing"
    );
    assert!(dash.agents().editor.as_ref().unwrap().model_drag.is_some());

    // Drag it over the first row: the chain *draws* in the order a release
    // would commit, so what is under the pointer is the answer.
    assert!(dash.handle_agents_mouse(at(drag, grips[0].1)));
    let screen = text(&mut dash);
    let seen = |name: &str| {
        screen
            .find(name)
            .unwrap_or_else(|| panic!("{name} is not drawn"))
    };
    assert!(
        seen("charlie-model") < seen("alfa-model"),
        "the held entry draws where it would land"
    );
    assert_eq!(
        models_of(&mut dash, "work"),
        chain,
        "still nothing written mid-drag"
    );

    // Release: now it is written, and the cursor stayed on the entry.
    assert!(dash.handle_agents_mouse(at(up, grips[0].1)));
    assert_eq!(
        models_of(&mut dash, "work"),
        vec![
            "charlie-model".to_string(),
            "alfa-model".to_string(),
            "bravo-model".to_string(),
        ],
        "lift-and-insert, not a swap: alfa and bravo both shift down one"
    );
    assert_eq!(dash.agents().editor.as_ref().unwrap().cursor, 0);
    assert!(dash.agents().editor.as_ref().unwrap().model_drag.is_none());

    // A drag that ends where it began costs nothing - not even an undo entry,
    // so one undo still reaches the order from before the first drop.
    let _ = draw(&mut dash, 160, 50);
    let grips = dash.agents().editor.as_ref().unwrap().hit.grips.clone();
    assert!(dash.handle_agents_mouse(at(down, grips[1].1)));
    assert!(dash.handle_agents_mouse(at(up, grips[1].1)));
    assert!(dash.agents().editor.as_mut().unwrap().undo());
    assert_eq!(models_of(&mut dash, "work"), chain);

    // Another button pressed part-way through a drag is not the drag's: it
    // takes its ordinary meaning and the held entry stays held.
    assert!(dash.handle_agents_mouse(at(down, grips[2].1)));
    assert!(!dash.editor_inspector_mouse(mouse(
        MouseEventKind::Down(MouseButton::Right),
        grips[0].1.x,
        grips[0].1.y
    )));
    assert!(dash.agents().editor.as_ref().unwrap().model_drag.is_some());
    assert!(dash.handle_agents_mouse(at(up, grips[2].1)));

    // The rows are rebuilt from the document every frame, and the document can
    // move under a held button (an undo, a reload). A drag whose indices no
    // longer fit the chain draws it untouched rather than panicking.
    dash.agents().editor.as_mut().unwrap().model_drag = Some(ModelDrag { from: 0, to: 9 });
    let screen = text(&mut dash);
    let seen = |name: &str| screen.find(name).expect("the chain is drawn");
    assert!(seen("alfa-model") < seen("bravo-model"), "{screen}");
    dash.agents().editor.as_mut().unwrap().model_drag = None;

    let _ = std::fs::remove_dir_all(&root);
}

/// The grip is a target, not the row. A press anywhere else on a model row is
/// the ordinary row click, and the drag machinery stays out of the way of
/// selecting the model id as text.
#[test]
fn pressing_a_model_row_off_its_grip_starts_no_drag() {
    let (mut dash, root) = dashboard("model_drag_off_grip");
    open_stage(&mut dash, "own", "work", StageTab::Model);
    let chain = vec!["alfa-model".to_string(), "bravo-model".to_string()];
    assert!(dash.editor_mutate(|d| d.set_models("work", &chain)));
    let _ = draw(&mut dash, 160, 50);
    let editor = dash.agents().editor.as_ref().unwrap();
    let grips = editor.hit.grips.clone();
    let rows = editor.hit.rows.clone();
    let press = |col: u16, row: u16| mouse(MouseEventKind::Down(MouseButton::Left), col, row);

    // Two cells to the right of the grip is the label, and further right the
    // value: both are ordinary row picks.
    let cell = grips[1].1;
    assert!(dash.handle_agents_mouse(press(cell.x + cell.width, cell.y)));
    assert!(dash.agents().editor.as_ref().unwrap().model_drag.is_none());
    assert_eq!(dash.agents().editor.as_ref().unwrap().cursor, 1);

    // A drag with nothing held is not the inspector's: it falls through to
    // the text selection behind it.
    assert!(!dash.editor_inspector_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        cell.x,
        cell.y
    )));
    assert!(!dash.editor_inspector_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        cell.x,
        cell.y
    )));

    // A drag off the rows entirely keeps the entry where it was picked up,
    // so letting go outside the list is a cancel rather than a random move.
    assert!(dash.handle_agents_mouse(press(cell.x, cell.y)));
    assert!(dash.handle_agents_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        cell.x,
        rows[rows.len() - 1] + 1
    )));
    assert!(dash.handle_agents_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        cell.x,
        rows[rows.len() - 1] + 1
    )));
    assert_eq!(models_of(&mut dash, "work"), chain);

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
    // A button row is its label, nothing in front of it.
    let screen = text(&mut dash);
    assert!(screen.contains("  Add a region"), "{screen}");
    assert!(!screen.contains("▸ Add a region"), "{screen}");
    assert!(screen.contains("Back to the shared layout"), "{screen}");
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

/// Tab and Shift-Tab walk a stage's tabs both ways and round the ends, on
/// a stage panel only (elsewhere Tab goes back to the canvas), Esc goes
/// back to the graph and Tab from there returns to the inspector; the
/// strip spells the tabs out when the inspector is wide enough and
/// shortens them when it is not.
#[test]
fn tab_and_shift_tab_walk_a_stages_tabs_and_the_strip_fits() {
    let (mut dash, root) = dashboard("tab_keys");
    open_stage(&mut dash, "own", "work", StageTab::Behaviour);
    let tab = |dash: &mut Dashboard| match &dash.agents().editor.as_ref().unwrap().panel {
        Panel::Stage { tab, .. } => *tab,
        other => panic!("not a stage: {other:?}"),
    };
    let focus = |dash: &mut Dashboard| dash.agents().editor.as_ref().unwrap().focus;
    dash.handle_key(key(KeyCode::Tab));
    assert_eq!(tab(&mut dash), StageTab::Io);
    dash.handle_key(key(KeyCode::BackTab));
    assert_eq!(tab(&mut dash), StageTab::Behaviour);
    dash.handle_key(key(KeyCode::BackTab));
    assert_eq!(tab(&mut dash), StageTab::Context, "round the end");
    dash.handle_key(key(KeyCode::Tab));
    assert_eq!(tab(&mut dash), StageTab::Behaviour);
    assert_eq!(focus(&mut dash), Focus::Inspector);
    assert_eq!(dash.agents().editor.as_ref().unwrap().cursor, 0);
    // Esc goes back to the graph; Tab (or Shift-Tab) from there returns to
    // the inspector on the same tab.
    dash.handle_key(key(KeyCode::Esc));
    assert_eq!(focus(&mut dash), Focus::Canvas);
    dash.handle_key(key(KeyCode::BackTab));
    assert_eq!(focus(&mut dash), Focus::Inspector);
    assert_eq!(tab(&mut dash), StageTab::Behaviour);
    // Side by side the inspector is wide enough for the full titles and
    // the hint names the keys; a small terminal hands the inspector the
    // whole width and gets the short titles, on one line.
    let wide = text(&mut dash);
    assert!(wide.contains("2 Inputs & outputs"), "{wide}");
    assert!(wide.contains("3 Models & tools"), "{wide}");
    assert!(wide.contains("4 Context & tools"), "{wide}");
    assert!(wide.contains("1-4 tab"), "{wide}");
    assert!(wide.contains("←→ change"), "{wide}");
    let small = rendered_buffer(&draw(&mut dash, 60, 40));
    assert!(small.contains("2 In & out"), "{small}");
    assert!(small.contains("3 Models "), "{small}");
    // Off a stage there are no tabs: the hint says so and Tab goes back to
    // the canvas.
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .view
        .clear_selection();
    dash.agents().editor.as_mut().unwrap().sync_panel();
    dash.agents().editor.as_mut().unwrap().focus = Focus::Inspector;
    let agent = text(&mut dash);
    assert!(agent.contains("←→ change"), "{agent}");
    assert!(agent.contains("tab canvas"), "{agent}");
    dash.handle_key(key(KeyCode::Tab));
    assert_eq!(focus(&mut dash), Focus::Canvas);
    {
        let editor = dash.agents().editor.as_mut().unwrap();
        editor.view.select_stage("work");
        editor.sync_panel();
        editor.focus = Focus::Inspector;
    }
    // A window has no tabs: the arrows change the row in it.
    dash.editor_add_region("notes");
    dash.handle_key(key(KeyCode::Esc));
    dash.handle_key(key(KeyCode::Char('2')));
    goto(&mut dash, FieldId::IoRegionRow("notes".into()));
    dash.handle_key(key(KeyCode::Enter));
    assert!(dash.agents().editor.as_ref().unwrap().modal.is_some());
    assert!(dash.agents().editor.as_ref().unwrap().panel_tab().is_none());
    dash.handle_key(key(KeyCode::Right));
    assert!(dash.agents().editor.as_ref().unwrap().modal.is_some());
    dash.handle_key(key(KeyCode::Esc));
    assert_eq!(tab(&mut dash), StageTab::Io);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_helpers_are_inert_off_their_panels_and_rows() {
    let (mut dash, root) = dashboard("panel_corners");
    open_stage(&mut dash, "own", "work", StageTab::Behaviour);
    // h/l on a text row or a button change nothing.
    goto(&mut dash, FieldId::StageDescription);
    dash.handle_key(key(KeyCode::Char('l')));
    goto(&mut dash, FieldId::EditPrompts);
    dash.handle_key(key(KeyCode::Char('h')));
    assert!(dash.agents().editor.as_ref().unwrap().overlay.is_none());
    // Enter on a plain status row opens nothing.
    dash.handle_key(key(KeyCode::Char('4')));
    goto(&mut dash, FieldId::ContextStatus);
    dash.handle_key(key(KeyCode::Enter));
    assert!(!picker_open(&mut dash));
    // The region-scoped number helper answers "mine" for a region field
    // even off a region panel, and does nothing.
    assert!(dash.editor_set_number_more(&FieldId::RegionBudget, None));
    dash.editor_set_toggle_more(&FieldId::StageMode, true);
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

/// The file a prompt is handed over in is private and its name is not
/// knowable in advance. A fixed name like
/// `<temp>/leviath-dash-prompts/<stage>-system.md` sits on a directory this
/// process does not own, so another local user can create it first and point
/// it wherever they like.
#[test]
fn handoff_files_are_private_and_unpredictable() {
    let (mut dash, root) = dashboard("prompts_private_handoff");
    dash.external_edit_dir = root.join("scratch");
    open_stage(&mut dash, "own", "work", StageTab::Behaviour);
    goto(&mut dash, FieldId::EditPrompts);
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::F(2)));
    let first = dash.take_external_edit().unwrap().path;
    dash.handle_key(key(KeyCode::F(2)));
    let second = dash.take_external_edit().unwrap().path;
    let predictable = root.join("scratch").join("work-system.md");
    assert_ne!(first, predictable, "the old fixed path");
    assert_ne!(first, second, "two handoffs, one name");
    assert!(
        first.starts_with(root.join("scratch")),
        "{}",
        first.display()
    );
    let name = first.file_name().unwrap().to_string_lossy();
    assert!(
        !name.contains("work"),
        "the stage name is not part of the path: {name}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&first).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{mode:o}");
        let dir_mode = std::fs::metadata(first.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "{dir_mode:o}");
    }
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

/// The tools chooser leads with the group tokens, labels each tool by where
/// it comes from, and keeps a name the manifest uses that this install lacks
/// (so it can be unticked) without doubling a group the manifest already
/// grants.
#[test]
fn the_tools_chooser_offers_groups_first_and_labels_sources() {
    let (mut dash, root) = dashboard("tool_choices");
    let agent_dir = root.join("agents").join("own");
    std::fs::create_dir_all(agent_dir.join("tools")).unwrap();
    std::fs::write(
        agent_dir.join("tools").join("summarize.rhai"),
        "// @tool summarize\n// @description sums\n1",
    )
    .unwrap();
    open_editor_on(&mut dash, "own");
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .doc
        .set_tools("work", &["@builtin".into(), "ghost_tool".into()])
        .unwrap();
    // Three MCP servers, in each state a server can be in.
    let mut mcp = super::McpCatalog::new();
    mcp.insert(
        "github".to_string(),
        super::McpServerTools::Listed(vec!["create_issue".to_string(), "search".to_string()]),
    );
    mcp.insert(
        "flaky".to_string(),
        super::McpServerTools::Failed("boom".to_string()),
    );
    mcp.insert("slow".to_string(), super::McpServerTools::Pending);
    mcp.insert(
        "one".to_string(),
        super::McpServerTools::Listed(vec!["only.tool".to_string()]),
    );
    let editor = dash.agents().editor.as_ref().unwrap();
    let choices = super::choices::tool_choices(&editor.dir, "own", &editor.doc, &mcp);

    let names: Vec<&str> = choices.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        &names[..5],
        &["@all", "@builtin", "@subagent", "@scripts", "@mcp"],
        "{names:?}"
    );
    // The servers follow the groups, as connectors, each saying where its
    // tool list stands; their tools sort in with the rest by their
    // advertised name.
    assert_eq!(
        &names[5..9],
        &["flaky", "github", "one", "slow"],
        "{names:?}"
    );
    assert!(choices[5..9].iter().all(|c| c.connector));
    assert!(choices[..5].iter().all(|c| !c.connector));
    assert!(names.contains(&"github__create_issue"), "{names:?}");
    assert!(names.contains(&"github__search"), "{names:?}");
    assert!(names.contains(&"one__only_tool"), "{names:?}");
    assert!(!names.iter().any(|n| n.starts_with("slow__")), "{names:?}");
    assert_eq!(names.iter().filter(|n| **n == "@builtin").count(), 1);
    let detail = |name: &str| {
        choices
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.detail.clone())
            .unwrap_or_else(|| panic!("{name} offered: {names:?}"))
    };
    assert_eq!(
        detail("@builtin"),
        leviath_core::blueprint::ToolGroup::Builtin.describe()
    );
    assert_eq!(detail("read_file"), "built in");
    assert_eq!(detail("spawn_agent"), "sub-agent tool");
    assert_eq!(detail("summarize"), "this agent's script");
    assert_eq!(
        detail("ghost_tool"),
        "named by this agent, not found on this install"
    );
    assert!(
        detail("github").contains("2 tools now"),
        "{}",
        detail("github")
    );
    assert!(detail("one").contains("1 tool now"), "{}", detail("one"));
    assert!(detail("slow").contains("asking it"), "{}", detail("slow"));
    assert!(detail("flaky").contains("boom"), "{}", detail("flaky"));
    assert_eq!(detail("github__search"), "github's tool, over MCP");
    // Past the groups and the servers the list is alphabetical.
    let rest: Vec<&str> = names[9..].to_vec();
    let mut sorted = rest.clone();
    sorted.sort_unstable();
    assert_eq!(rest, sorted);
    let _ = std::fs::remove_dir_all(&root);
}
