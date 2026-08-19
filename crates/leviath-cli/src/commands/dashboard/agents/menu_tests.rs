//! The canvas's right-click menu, a click off everything, and the hint
//! bars that name every control: driven by mouse and keys, frames read
//! back.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};

use super::context_menu::{ContextMenu, MenuAction};
use super::editor::{Focus, Overlay};
use super::inspector::{FieldId, Panel, StageTab};
use super::tests::{dashboard, draw, key, mouse, open_editor_on, text, type_str};
use crate::commands::dashboard::state::Dashboard;
use crate::commands::dashboard::test_support::rendered_buffer;
use crate::tui::flowgraph::MenuTarget;

/// A press and release of `button` at a cell.
fn click(dash: &mut Dashboard, button: MouseButton, x: u16, y: u16) {
    dash.handle_mouse(mouse(MouseEventKind::Down(button), x, y));
    dash.handle_mouse(mouse(MouseEventKind::Up(button), x, y));
}

/// A cell inside the box of `stage`, after a draw.
fn inside(dash: &mut Dashboard, stage: &str) -> (u16, u16) {
    let (x, y, _, _) = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .view
        .node_rect(stage)
        .expect("drawn");
    (x as u16 + 2, y as u16 + 1)
}

/// An empty cell of the canvas: the bottom-left corner of the graph pane.
fn empty_cell(dash: &mut Dashboard) -> (u16, u16) {
    let rect = dash
        .pane_rects
        .iter()
        .find(|(id, _)| *id == crate::commands::dashboard::types::PaneId::AgentEditorGraph)
        .map(|(_, r)| *r)
        .expect("the canvas is registered");
    (rect.x + 2, rect.y + rect.height - 3)
}

fn menu(dash: &mut Dashboard) -> Option<ContextMenu> {
    dash.agents().editor.as_ref().unwrap().menu.clone()
}

fn panel(dash: &mut Dashboard) -> Panel {
    dash.agents().editor.as_ref().unwrap().panel.clone()
}

#[test]
fn a_click_off_everything_clears_the_selection_but_a_pan_keeps_it() {
    let (mut dash, root) = dashboard("click_off");
    open_editor_on(&mut dash, "own");
    draw(&mut dash, 160, 50);
    let (x, y) = inside(&mut dash, "finish");
    click(&mut dash, MouseButton::Left, x, y);
    assert!(matches!(panel(&mut dash), Panel::Stage { .. }));
    // A press and release on empty canvas: nothing selected, the agent panel.
    let (ex, ey) = empty_cell(&mut dash);
    click(&mut dash, MouseButton::Left, ex, ey);
    assert_eq!(panel(&mut dash), Panel::Agent);
    // Select again, then pan from empty canvas: the selection stays.
    click(&mut dash, MouseButton::Left, x, y);
    dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), ex, ey));
    dash.handle_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        ex + 4,
        ey - 2,
    ));
    dash.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), ex + 4, ey - 2));
    assert!(matches!(panel(&mut dash), Panel::Stage { .. }));
    // A drag that goes nowhere is still a click.
    dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), ex, ey));
    dash.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), ex, ey));
    dash.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), ex, ey));
    assert_eq!(panel(&mut dash), Panel::Agent);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_right_click_opens_a_menu_for_what_is_under_it() {
    let (mut dash, root) = dashboard("menus");
    open_editor_on(&mut dash, "own");
    draw(&mut dash, 160, 50);
    // A stage: its menu, drawn where the click landed, with the keys named.
    let (x, y) = inside(&mut dash, "work");
    click(&mut dash, MouseButton::Right, x, y);
    let m = menu(&mut dash).expect("a stage menu");
    assert_eq!(m.title, "Stage · work");
    assert_eq!(m.at, (x, y));
    let screen = text(&mut dash);
    assert!(screen.contains("Stage · work"), "{screen}");
    assert!(screen.contains("Add a stage after it"), "{screen}");
    assert!(
        screen.contains("esc close · ↑↓ move · enter do it"),
        "{screen}"
    );
    // Letters and the wheel do nothing to it; ↑↓ move, clamped.
    dash.handle_key(key(KeyCode::Char('q')));
    dash.handle_mouse(mouse(MouseEventKind::ScrollDown, x, y));
    assert!(menu(&mut dash).is_some());
    dash.handle_key(key(KeyCode::Up));
    assert_eq!(menu(&mut dash).unwrap().cursor, 0);
    for _ in 0..9 {
        dash.handle_key(key(KeyCode::Down));
    }
    assert_eq!(menu(&mut dash).unwrap().cursor, 5);
    dash.handle_key(key(KeyCode::Char('k')));
    dash.handle_key(key(KeyCode::Char('j')));
    assert_eq!(menu(&mut dash).unwrap().cursor, 5);
    // Esc closes it; a click elsewhere closes it and goes no further (the
    // stage selected with the left button stays selected).
    dash.handle_key(key(KeyCode::Esc));
    assert!(menu(&mut dash).is_none());
    click(&mut dash, MouseButton::Left, x, y);
    click(&mut dash, MouseButton::Right, x, y);
    let (ex, ey) = empty_cell(&mut dash);
    dash.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), ex, ey));
    assert!(menu(&mut dash).is_none());
    assert!(
        matches!(panel(&mut dash), Panel::Stage { .. }),
        "the click only closed the menu"
    );
    dash.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), ex, ey));
    // Empty canvas: the canvas menu; Enter on "fit" runs and closes.
    click(&mut dash, MouseButton::Right, ex, ey);
    let m = menu(&mut dash).expect("a canvas menu");
    assert_eq!(m.title, "Canvas");
    assert!(matches!(m.items[0].action, MenuAction::AddStageAt(..)));
    dash.handle_key(key(KeyCode::Down));
    dash.handle_key(key(KeyCode::Enter));
    assert!(menu(&mut dash).is_none());
    // A path: its menu.
    let (fx, fy, _, fb) = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .view
        .node_rect("finish")
        .unwrap();
    let (wx, wy, wr, wb) = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .view
        .node_rect("work")
        .unwrap();
    let _ = (wx, fb, wb);
    // The path runs between the boxes at mid height (left-to-right).
    let mid = ((wr + fx) / 2) as u16;
    let row = ((wy + fy) / 2 + 1) as u16;
    click(&mut dash, MouseButton::Right, mid, row);
    let m = menu(&mut dash).expect("a path menu under the line");
    assert_eq!(m.title, "Path · work → finish");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(
        panel(&mut dash),
        Panel::Edge {
            from: "work".into(),
            to: "finish".into()
        }
    );
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().focus,
        Focus::Inspector
    );
    // A worker box of another agent has no menu, only a message.
    dash.editor_open_menu(&MenuTarget::Node("ext:researcher".into()), (1, 1));
    assert!(menu(&mut dash).is_none());
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .message
            .as_deref()
            .is_some_and(|m| m.contains("separate agent"))
    );
    // A right drag is not the canvas's (it would box-select), nor is plain
    // motion.
    assert!(
        !dash
            .agents()
            .editor
            .as_mut()
            .unwrap()
            .view
            .handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Right), x, y))
    );
    // The menu draws clamped inside a small area.
    dash.editor_open_menu(&MenuTarget::Node("work".into()), (79, 23));
    let screen = rendered_buffer(&draw(&mut dash, 80, 24));
    assert!(screen.contains("Stage · work"), "{screen}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn every_menu_row_does_what_its_key_does() {
    let (mut dash, root) = dashboard("menu_rows");
    open_editor_on(&mut dash, "own");
    draw(&mut dash, 160, 50);
    let (x, y) = inside(&mut dash, "work");
    let pick = |dash: &mut Dashboard, row: usize| {
        click(dash, MouseButton::Right, x, y);
        let m = menu(dash).unwrap();
        let drawn = {
            draw(dash, 160, 50);
            menu(dash).unwrap().drawn
        };
        assert_eq!(m.items.len(), 6);
        // A click on the row picks it.
        dash.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            drawn.x + 2,
            drawn.y + 1 + row as u16,
        ));
        assert!(menu(dash).is_none());
    };
    // Edit.
    pick(&mut dash, 0);
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().focus,
        Focus::Inspector
    );
    assert!(matches!(panel(&mut dash), Panel::Stage { name, .. } if name == "work"));
    dash.handle_key(key(KeyCode::Esc));
    // Connect to…: the chooser.
    pick(&mut dash, 1);
    assert!(dash.agents().editor.as_ref().unwrap().picker.is_some());
    dash.handle_key(key(KeyCode::Esc));
    // Add a stage after it: the name prompt, placed after the rightmost.
    pick(&mut dash, 2);
    assert!(dash.agents().editor.as_ref().unwrap().add_stage.is_some());
    type_str(&mut dash, "next");
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().doc.stage_names(),
        ["work", "next", "finish"]
    );
    dash.handle_key(key(KeyCode::Esc));
    draw(&mut dash, 160, 50);
    // Rename: the name row is being typed.
    pick(&mut dash, 3);
    assert!(matches!(
        dash.agents().editor.as_ref().unwrap().line,
        Some((FieldId::StageName, _))
    ));
    dash.handle_key(key(KeyCode::Esc));
    dash.handle_key(key(KeyCode::Esc));
    draw(&mut dash, 160, 50);
    // Edit prompts: the overlay.
    pick(&mut dash, 4);
    assert!(matches!(
        dash.agents().editor.as_ref().unwrap().overlay,
        Some(Overlay::Prompts(_))
    ));
    dash.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
    dash.handle_key(key(KeyCode::Esc));
    draw(&mut dash, 160, 50);
    // Delete stage: asks.
    pick(&mut dash, 5);
    assert!(dash.pending_confirm.is_some());
    dash.handle_key(key(KeyCode::Char('n')));
    // The canvas menu: add a stage at the click, turn, definition.
    let (ex, ey) = empty_cell(&mut dash);
    click(&mut dash, MouseButton::Right, ex, ey);
    let MenuAction::AddStageAt(wx, wy) = menu(&mut dash).unwrap().items[0].action.clone() else {
        panic!("the first row adds a stage");
    };
    dash.handle_key(key(KeyCode::Enter));
    assert_eq!(
        dash.agents().editor.as_ref().unwrap().place_next,
        Some((wx, wy))
    );
    type_str(&mut dash, "here");
    dash.handle_key(key(KeyCode::Enter));
    let at = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .view
        .positions()
        .get("here")
        .copied()
        .expect("placed");
    assert!(
        (at.0 - wx).abs() < 1e-6 && (at.1 - wy).abs() < 1e-6,
        "{at:?} vs {wx},{wy}"
    );
    assert!(dash.agents().editor.as_ref().unwrap().place_next.is_none());
    // Esc on the prompt also forgets the spot.
    dash.handle_key(key(KeyCode::Esc));
    click(&mut dash, MouseButton::Right, ex, ey);
    dash.handle_key(key(KeyCode::Enter));
    dash.handle_key(key(KeyCode::Esc));
    assert!(dash.agents().editor.as_ref().unwrap().add_stage.is_none());
    dash.editor_menu_action(MenuAction::Rotate);
    dash.editor_menu_action(MenuAction::Definition);
    assert!(matches!(
        dash.agents().editor.as_ref().unwrap().overlay,
        Some(Overlay::Definition { .. })
    ));
    dash.handle_key(key(KeyCode::Esc));
    // A path row: delete it.
    dash.editor_menu_action(MenuAction::DeletePath("work".into(), "finish".into()));
    assert!(
        dash.agents()
            .editor
            .as_ref()
            .unwrap()
            .doc
            .edge("work", "finish")
            .is_none()
    );
    // The menu helpers are inert with no menu, and with no editor.
    assert!(!dash.editor_menu_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 1)));
    dash.editor_menu_key(&key(KeyCode::Enter));
    dash.close_editor();
    assert!(!dash.editor_menu_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 1)));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_bars_name_the_controls_on_every_screen() {
    let (mut dash, root) = dashboard("bars");
    // The run list calls the Agents screen out next to "new run", and the
    // empty state too.
    let screen = text(&mut dash);
    assert!(screen.contains("[n] new run  [a] agents"), "{screen}");
    assert!(screen.contains("`a` to build an agent"), "{screen}");
    // The catalog, its filter, and the chooser.
    dash.handle_key(key(KeyCode::Char('a')));
    let screen = text(&mut dash);
    assert!(
        screen.contains("esc back · ↑↓ select · enter edit · n new"),
        "{screen}"
    );
    dash.handle_key(key(KeyCode::Char('/')));
    let screen = text(&mut dash);
    assert!(
        screen.contains("esc clear · type filter · enter keep"),
        "{screen}"
    );
    dash.handle_key(key(KeyCode::Esc));
    dash.handle_key(key(KeyCode::Char('n')));
    let screen = text(&mut dash);
    assert!(screen.contains("↑↓ template · type the name"), "{screen}");
    dash.handle_key(key(KeyCode::Esc));
    // The wheel over the catalog list moves the cursor; off it, or with the
    // chooser open, it does not.
    draw(&mut dash, 160, 50);
    let list = dash.agents().list_area;
    let before = dash.agents().catalog.selected;
    dash.handle_mouse(mouse(MouseEventKind::ScrollDown, list.x + 2, list.y + 2));
    assert_eq!(dash.agents().catalog.selected, before + 1);
    dash.handle_mouse(mouse(MouseEventKind::ScrollUp, list.x + 2, list.y + 2));
    assert_eq!(dash.agents().catalog.selected, before);
    assert!(!dash.catalog_wheel(mouse(
        MouseEventKind::ScrollDown,
        list.x + list.width + 5,
        list.y
    )));
    assert!(!dash.catalog_wheel(mouse(
        MouseEventKind::Down(MouseButton::Left),
        list.x + 2,
        list.y + 2
    )));
    dash.handle_key(key(KeyCode::Char('n')));
    assert!(!dash.catalog_wheel(mouse(MouseEventKind::ScrollDown, list.x + 2, list.y + 2)));
    dash.handle_key(key(KeyCode::Esc));
    // The editor: the canvas, the inspector (Esc says where it goes), a
    // pushed panel, the choosers, the prompts.
    open_editor_on(&mut dash, "own");
    let screen = text(&mut dash);
    assert!(screen.contains("esc close · ^s save · ? help"), "{screen}");
    assert!(screen.contains("right-click menu"), "{screen}");
    dash.handle_key(key(KeyCode::Tab));
    let screen = text(&mut dash);
    assert!(screen.contains("esc canvas · ^s save"), "{screen}");
    assert!(screen.contains("x remove"), "{screen}");
    dash.editor_add_region("r");
    let screen = text(&mut dash);
    assert!(screen.contains("esc back · ^s save"), "{screen}");
    dash.handle_key(key(KeyCode::Esc));
    dash.agents()
        .editor
        .as_mut()
        .unwrap()
        .view
        .select_stage("work");
    dash.agents().editor.as_mut().unwrap().sync_panel();
    dash.agents().editor.as_mut().unwrap().panel = Panel::Stage {
        name: "work".into(),
        tab: StageTab::Model,
    };
    let at = dash
        .agents()
        .editor
        .as_ref()
        .unwrap()
        .fields()
        .iter()
        .position(|f| f.id == FieldId::ToolSet)
        .unwrap();
    dash.agents().editor.as_mut().unwrap().cursor = at;
    dash.handle_key(key(KeyCode::Enter));
    let screen = text(&mut dash);
    assert!(
        screen.contains("space pick / drop · enter keep"),
        "{screen}"
    );
    dash.handle_key(key(KeyCode::Esc));
    dash.agents().editor.as_mut().unwrap().cursor = 0;
    dash.handle_key(key(KeyCode::Enter));
    let screen = text(&mut dash);
    assert!(
        screen.contains("esc cancel · type search · ↑↓ move · enter choose"),
        "{screen}"
    );
    dash.handle_key(key(KeyCode::Esc));
    let _ = std::fs::remove_dir_all(&root);
}
