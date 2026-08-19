//! The canvas's right-click menu: what can be done to the box, the path,
//! or the empty canvas under the pointer. It is a small list drawn where
//! the click landed; `↑`/`↓` and `Enter` work it, `Esc` or a click
//! anywhere else closes it, and a click on a row picks it. Every row does
//! what a key on the canvas does, so the menu discloses the keys rather
//! than adding to them.

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use super::super::state::Dashboard;
use super::super::theme::*;
use super::editor::Focus;
use super::inspector::{FieldId, Panel, StageTab};
use crate::tui::flowgraph::MenuTarget;
use crate::tui::widgets::line_edit::LineEdit;

/// What a menu row does.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::commands::dashboard) enum MenuAction {
    /// Select the stage and put the keys on its inspector.
    EditStage(String),
    /// Select the stage and open the connect chooser from it.
    ConnectFrom(String),
    /// Add a stage after this one (asks its name).
    AddStageAfter(String),
    /// Select the stage and start typing its new name.
    RenameStage(String),
    /// Select the stage and open its prompts.
    EditPrompts(String),
    /// Delete the stage (asks first).
    DeleteStage(String),
    /// Select the path and put the keys on its inspector.
    EditPath(String, String),
    /// Delete the path.
    DeletePath(String, String),
    /// Add a stage where the canvas was clicked (asks its name).
    AddStageAt(f64, f64),
    /// Fit the whole graph on screen.
    Fit,
    /// Turn the graph.
    Rotate,
    /// Show the file that will be saved.
    Definition,
}

/// One row of the menu.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::commands::dashboard) struct MenuItem {
    pub(in crate::commands::dashboard) label: &'static str,
    /// The key that does the same on the canvas, shown dim at the right.
    pub(in crate::commands::dashboard) key: &'static str,
    pub(in crate::commands::dashboard) action: MenuAction,
}

/// The open menu.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::commands::dashboard) struct ContextMenu {
    pub(in crate::commands::dashboard) title: String,
    pub(in crate::commands::dashboard) items: Vec<MenuItem>,
    pub(in crate::commands::dashboard) cursor: usize,
    /// Where the click landed (screen cells); the menu opens there.
    pub(in crate::commands::dashboard) at: (u16, u16),
    /// Where the last frame drew it, for the mouse.
    pub(in crate::commands::dashboard) drawn: Rect,
}

impl ContextMenu {
    /// The menu for what a right click landed on, or `None` when nothing
    /// can be done there (a worker box belongs to another agent).
    pub(in crate::commands::dashboard) fn for_target(
        target: &MenuTarget,
        at: (u16, u16),
    ) -> Option<Self> {
        let (title, items) = match target {
            MenuTarget::Node(id) if id.starts_with("ext:") => return None,
            MenuTarget::Node(name) => (
                format!("Stage · {name}"),
                vec![
                    MenuItem {
                        label: "Edit",
                        key: "enter",
                        action: MenuAction::EditStage(name.clone()),
                    },
                    MenuItem {
                        label: "Connect to…",
                        key: "c",
                        action: MenuAction::ConnectFrom(name.clone()),
                    },
                    MenuItem {
                        label: "Add a stage after it",
                        key: "a",
                        action: MenuAction::AddStageAfter(name.clone()),
                    },
                    MenuItem {
                        label: "Rename",
                        key: "",
                        action: MenuAction::RenameStage(name.clone()),
                    },
                    MenuItem {
                        label: "Edit prompts",
                        key: "",
                        action: MenuAction::EditPrompts(name.clone()),
                    },
                    MenuItem {
                        label: "Delete stage",
                        key: "x",
                        action: MenuAction::DeleteStage(name.clone()),
                    },
                ],
            ),
            MenuTarget::Edge(edge) => (
                format!("Path · {} → {}", edge.from, edge.to),
                vec![
                    MenuItem {
                        label: "Edit",
                        key: "enter",
                        action: MenuAction::EditPath(edge.from.clone(), edge.to.clone()),
                    },
                    MenuItem {
                        label: "Delete path",
                        key: "x",
                        action: MenuAction::DeletePath(edge.from.clone(), edge.to.clone()),
                    },
                ],
            ),
            MenuTarget::Pane { x, y } => (
                "Canvas".to_string(),
                vec![
                    MenuItem {
                        label: "Add a stage here",
                        key: "a",
                        action: MenuAction::AddStageAt(*x, *y),
                    },
                    MenuItem {
                        label: "Fit the graph",
                        key: "f",
                        action: MenuAction::Fit,
                    },
                    MenuItem {
                        label: "Turn the graph",
                        key: "r",
                        action: MenuAction::Rotate,
                    },
                    MenuItem {
                        label: "Show the definition",
                        key: "v",
                        action: MenuAction::Definition,
                    },
                ],
            ),
        };
        Some(Self {
            title,
            items,
            cursor: 0,
            at,
            drawn: Rect::default(),
        })
    }

    /// The row under a screen position, if the menu was drawn there.
    fn row_at(&self, column: u16, row: u16) -> Option<usize> {
        let inner = Rect {
            x: self.drawn.x + 1,
            y: self.drawn.y + 1,
            width: self.drawn.width.saturating_sub(2),
            height: self.drawn.height.saturating_sub(2),
        };
        (column >= inner.x
            && column < inner.x + inner.width
            && row >= inner.y
            && row < inner.y + inner.height)
            .then(|| (row - inner.y) as usize)
            .filter(|i| *i < self.items.len())
    }

    /// Draw the menu at its anchor, kept inside `area`.
    pub(in crate::commands::dashboard) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let widest = self
            .items
            .iter()
            .map(|i| i.label.chars().count() + 2 + i.key.chars().count())
            .max()
            .unwrap_or(0)
            .max(self.title.chars().count() + 2);
        let width = (widest as u16 + 6).min(area.width);
        let height = (self.items.len() as u16 + 2).min(area.height);
        let x = self
            .at
            .0
            .min(area.x + area.width.saturating_sub(width))
            .max(area.x);
        let y = self
            .at
            .1
            .min(area.y + area.height.saturating_sub(height))
            .max(area.y);
        let rect = Rect {
            x,
            y,
            width,
            height,
        };
        self.drawn = rect;
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(C_BORDER_FOCUS))
            .title(Span::styled(
                format!(" {} ", self.title),
                Style::default().fg(C_ACCENT).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let room = inner.width as usize;
        let lines: Vec<Line> = self
            .items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let on = i == self.cursor;
                let label_w = room.saturating_sub(2 + item.key.chars().count());
                let mut spans = vec![Span::styled(
                    if on { "› " } else { "  " },
                    Style::default().fg(C_ACCENT),
                )];
                spans.push(Span::styled(
                    format!("{:<label_w$}", item.label),
                    if on {
                        Style::default().fg(C_ACTIVE).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(C_WHITE)
                    },
                ));
                spans.push(Span::styled(item.key, Style::default().fg(C_DIM)));
                Line::from(spans)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

impl Dashboard {
    /// Open the menu for what a right click landed on; a worker box gets a
    /// message instead, the same one its inspector panel shows.
    pub(super) fn editor_open_menu(&mut self, target: &MenuTarget, at: (u16, u16)) {
        match ContextMenu::for_target(target, at) {
            Some(menu) => self.editor().menu = Some(menu),
            None => {
                self.editor().message =
                    Some("That box is a separate agent; edit it from the catalog".to_string());
            }
        }
    }

    /// Keys while the menu is open.
    pub(super) fn editor_menu_key(&mut self, key: &KeyEvent) {
        let Some(menu) = self.editor().menu.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.editor().menu = None,
            KeyCode::Up | KeyCode::Char('k') => menu.cursor = menu.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                menu.cursor = (menu.cursor + 1).min(menu.items.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                let action = menu.items[menu.cursor].action.clone();
                self.editor().menu = None;
                self.editor_menu_action(action);
            }
            _ => {}
        }
    }

    /// The mouse while the menu is open: a press on a row picks it, a
    /// press anywhere else closes the menu (and goes no further); the wheel
    /// and moves are ignored. Returns whether the menu took the event.
    pub(super) fn editor_menu_mouse(&mut self, event: MouseEvent) -> bool {
        let Some(menu) = self
            .agents()
            .editor
            .as_ref()
            .and_then(|editor| editor.menu.as_ref())
        else {
            return false;
        };
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Down(MouseButton::Right) => {
                let hit = menu.row_at(event.column, event.row);
                let action = hit.map(|i| menu.items[i].action.clone());
                self.editor().menu = None;
                if let Some(action) = action {
                    self.editor_menu_action(action);
                }
                true
            }
            // Releases and drags belong to the press that opened or closed
            // the menu; the wheel is swallowed so the canvas does not zoom
            // under an open menu.
            _ => true,
        }
    }

    /// Run a menu row: each one is the key it names.
    pub(super) fn editor_menu_action(&mut self, action: MenuAction) {
        match action {
            MenuAction::EditStage(name) => {
                let editor = self.editor();
                editor.view.select_stage(&name);
                editor.sync_panel();
                editor.focus = Focus::Inspector;
            }
            MenuAction::ConnectFrom(name) => {
                let editor = self.editor();
                editor.view.select_stage(&name);
                editor.sync_panel();
                self.editor_open_connect();
            }
            MenuAction::AddStageAfter(name) => {
                let editor = self.editor();
                editor.view.select_stage(&name);
                editor.sync_panel();
                editor.place_next = None;
                editor.add_stage = Some(LineEdit::new(String::new(), false));
            }
            MenuAction::RenameStage(name) => {
                let editor = self.editor();
                editor.view.select_stage(&name);
                editor.sync_panel();
                editor.panel = Panel::Stage {
                    name: name.clone(),
                    tab: StageTab::Behaviour,
                };
                editor.focus = Focus::Inspector;
                editor.cursor = editor
                    .fields()
                    .iter()
                    .position(|f| f.id == FieldId::StageName)
                    .unwrap_or(0);
                self.editor_activate();
            }
            MenuAction::EditPrompts(name) => {
                let editor = self.editor();
                editor.view.select_stage(&name);
                editor.sync_panel();
                editor.panel = Panel::Stage {
                    name,
                    tab: StageTab::Behaviour,
                };
                editor.focus = Focus::Inspector;
                self.editor_open_prompts();
            }
            MenuAction::DeleteStage(name) => {
                let editor = self.editor();
                editor.view.select_stage(&name);
                editor.sync_panel();
                self.editor_request_delete_stage(&name);
            }
            MenuAction::EditPath(from, to) => {
                let editor = self.editor();
                editor.view.select_edge(&from, &to);
                editor.sync_panel();
                editor.focus = Focus::Inspector;
            }
            MenuAction::DeletePath(from, to) => self.editor_delete_edge(&from, &to),
            MenuAction::AddStageAt(x, y) => {
                let editor = self.editor();
                editor.place_next = Some((x, y));
                editor.add_stage = Some(LineEdit::new(String::new(), false));
            }
            MenuAction::Fit => self.editor().view.fit(),
            MenuAction::Rotate => self.editor().view.rotate(),
            MenuAction::Definition => {
                self.editor().overlay = Some(super::editor::Overlay::Definition { scroll: 0 });
            }
        }
    }
}
