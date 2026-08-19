//! Keys and mouse on the editor: who gets them (a chooser, a line editor,
//! an overlay, the canvas, the inspector), and the canvas operations that
//! change the graph.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use super::super::state::Dashboard;
use super::editor::{Focus, Overlay, PickerFor};
use super::inspector::{Panel, StageTab};
use crate::tui::flowgraph::CanvasEvent;
use crate::tui::widgets::line_edit::{EditOutcome, LineEdit};
use crate::tui::widgets::picker::{Picker, PickerOption, PickerOutcome};

impl Dashboard {
    /// Keys while the editor is open.
    pub(in crate::commands::dashboard) fn handle_editor_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // A message answers one key; the next key clears it.
        self.editor().message = None;
        // Save works from anywhere in the editor, chooser and all: what is
        // in the document is what is saved. The prompts overlay is the one
        // place it means "apply these": its text is not in the document yet.
        if ctrl && key.code == KeyCode::Char('s') {
            if matches!(self.editor().overlay, Some(Overlay::Prompts(_))) {
                self.editor_prompts_key(&key);
            } else {
                self.editor_save();
            }
            return;
        }
        if self.editor().picker.is_some() {
            self.editor_picker_key(&key);
            return;
        }
        if self.editor().add_stage.is_some() {
            self.editor_add_stage_key(&key);
            return;
        }
        if self.editor().add_region.is_some() {
            self.editor_add_region_key(&key);
            return;
        }
        if self.editor().line.is_some() {
            self.editor_line_key(&key);
            return;
        }
        if self.editor().overlay.is_some() {
            self.editor_overlay_key(&key);
            return;
        }
        match key.code {
            KeyCode::Char('?') | KeyCode::F(1) => self.show_help = true,
            KeyCode::Char('u') => {
                if !self.editor().undo() {
                    self.editor().message = Some("Nothing to undo".to_string());
                }
            }
            KeyCode::Char('r') if ctrl => {
                if !self.editor().redo() {
                    self.editor().message = Some("Nothing to redo".to_string());
                }
            }
            KeyCode::Char('v') => {
                self.editor().overlay = Some(Overlay::Definition { scroll: 0 });
            }
            KeyCode::Char('p') => {
                let editor = self.editor();
                editor.problems_open = !editor.problems_open;
            }
            KeyCode::Tab => {
                let editor = self.editor();
                editor.focus = match editor.focus {
                    Focus::Canvas => Focus::Inspector,
                    Focus::Inspector => Focus::Canvas,
                };
            }
            _ => match self.editor().focus {
                Focus::Canvas => self.editor_canvas_key(key.code),
                Focus::Inspector => self.editor_inspector_key(key.code),
            },
        }
    }

    /// Keys on the canvas.
    fn editor_canvas_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.editor_close_requested(),
            KeyCode::Enter => {
                let editor = self.editor();
                editor.sync_panel();
                editor.focus = Focus::Inspector;
            }
            KeyCode::Char('a') => {
                self.editor().add_stage = Some(LineEdit::new(String::new(), false));
            }
            KeyCode::Char('c') => self.editor_open_connect(),
            KeyCode::Char('x') | KeyCode::Delete => self.editor_delete_selected(),
            KeyCode::Char('r') => {
                self.editor().view.rotate();
            }
            KeyCode::Char('f') => {
                self.editor().view.fit();
            }
            other => {
                let editor = self.editor();
                if editor.view.handle_key(other) {
                    editor.sync_panel();
                }
            }
        }
    }

    /// Keys on the inspector.
    fn editor_inspector_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                if self.editor().panel_anchor.is_some() {
                    self.editor_leave_region();
                } else {
                    self.editor().focus = Focus::Canvas;
                }
            }
            KeyCode::Char('x') | KeyCode::Delete => self.editor_remove_row(),
            KeyCode::Up | KeyCode::Char('k') => {
                let editor = self.editor();
                editor.cursor = editor.cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let editor = self.editor();
                let last = editor.fields().len().saturating_sub(1);
                editor.cursor = (editor.cursor + 1).min(last);
            }
            KeyCode::Home => self.editor().cursor = 0,
            KeyCode::End => {
                let editor = self.editor();
                editor.cursor = editor.fields().len().saturating_sub(1);
            }
            KeyCode::Enter => self.editor_activate(),
            KeyCode::Left | KeyCode::Char('h') => self.editor_adjust(-1),
            KeyCode::Right | KeyCode::Char('l') => self.editor_adjust(1),
            KeyCode::Char(c @ '1'..='3') => {
                let tab = StageTab::ALL[(c as usize) - ('1' as usize)];
                let editor = self.editor();
                if let Panel::Stage { name, .. } = &editor.panel {
                    editor.panel = Panel::Stage {
                        name: name.clone(),
                        tab,
                    };
                    editor.cursor = 0;
                }
            }
            _ => {}
        }
    }

    /// Keys while a field is being typed into.
    fn editor_line_key(&mut self, key: &KeyEvent) {
        let (id, mut line) = self.editor().line.take().expect("callers check");
        match line.handle_key(key) {
            EditOutcome::Pending => self.editor().line = Some((id, line)),
            EditOutcome::Cancel => {}
            EditOutcome::Commit => {
                let text = line.value().to_string();
                self.editor_commit_line(&id, &text);
            }
        }
    }

    /// Keys while the name of a new stage is being typed.
    fn editor_add_stage_key(&mut self, key: &KeyEvent) {
        let mut line = self.editor().add_stage.take().expect("callers check");
        match line.handle_key(key) {
            EditOutcome::Pending => self.editor().add_stage = Some(line),
            EditOutcome::Cancel => {}
            EditOutcome::Commit => {
                let name = line.value().trim().to_string();
                if name.is_empty() {
                    return;
                }
                let after = self.editor().panel_stage();
                if self.editor_mutate(|d| d.add_stage(&name, after.as_deref())) {
                    let editor = self.editor();
                    editor.view.select_stage(&name);
                    editor.view.reveal(&name);
                    editor.sync_panel();
                    editor.focus = Focus::Inspector;
                }
            }
        }
    }

    /// Keys while the name of a new region is being typed.
    fn editor_add_region_key(&mut self, key: &KeyEvent) {
        let mut line = self.editor().add_region.take().expect("callers check");
        match line.handle_key(key) {
            EditOutcome::Pending => self.editor().add_region = Some(line),
            EditOutcome::Cancel => {}
            EditOutcome::Commit => {
                let name = line.value().trim().to_string();
                if !name.is_empty() {
                    self.editor_add_region(&name);
                }
            }
        }
    }

    /// Keys on the chooser.
    fn editor_picker_key(&mut self, key: &KeyEvent) {
        let (purpose, mut picker) = self.editor().picker.take().expect("callers check");
        let outcome = picker.handle_key(key);
        self.editor_settle_picker(purpose, picker, outcome);
    }

    /// The mouse on the chooser, when one is open.
    pub(in crate::commands::dashboard) fn editor_picker_mouse(
        &mut self,
        event: MouseEvent,
        area: Rect,
    ) -> bool {
        let Some(editor) = self.agents().editor.as_mut() else {
            return false;
        };
        let Some((purpose, mut picker)) = editor.picker.take() else {
            return false;
        };
        let outcome = picker.handle_mouse(&event, area);
        self.editor_settle_picker(purpose, picker, outcome);
        true
    }

    /// A click on the inspector: the row under it takes the cursor and the
    /// keys; a click on a stage tab switches to it; a second click on the
    /// row the cursor is on opens it, like Enter.
    pub(in crate::commands::dashboard) fn editor_inspector_mouse(
        &mut self,
        event: MouseEvent,
    ) -> bool {
        let Some(editor) = self.agents().editor.as_mut() else {
            return false;
        };
        if editor.overlay.is_some() || editor.line.is_some() || editor.add_stage.is_some() {
            return false;
        }
        if event.kind != MouseEventKind::Down(MouseButton::Left) {
            return false;
        }
        let hit = editor.hit.clone();
        let inside = event.column >= hit.area.x
            && event.column < hit.area.x + hit.area.width
            && event.row >= hit.area.y
            && event.row < hit.area.y + hit.area.height;
        if !inside {
            return false;
        }
        if let Some((tab_row, tabs)) = &hit.tabs
            && event.row == *tab_row
            && let Some(i) = tabs
                .iter()
                .position(|(x0, x1)| event.column >= *x0 && event.column < *x1)
            && let Panel::Stage { name, .. } = &editor.panel
        {
            editor.panel = Panel::Stage {
                name: name.clone(),
                tab: StageTab::ALL[i],
            };
            editor.cursor = 0;
            editor.focus = Focus::Inspector;
            return true;
        }
        let was = (editor.focus, editor.cursor);
        editor.focus = Focus::Inspector;
        if let Some(i) = hit.rows.iter().position(|y| *y == event.row) {
            editor.cursor = i;
            if was == (Focus::Inspector, i) {
                self.editor_activate();
            }
        }
        true
    }

    fn editor_settle_picker(&mut self, purpose: PickerFor, picker: Picker, outcome: PickerOutcome) {
        match outcome {
            PickerOutcome::Pending => self.editor().picker = Some((purpose, picker)),
            PickerOutcome::Cancelled => {}
            PickerOutcome::ChosenMany(chosen) => self.editor_settle_tools(&chosen),
            PickerOutcome::Chosen(index) => {
                let value = picker.options[index].value.clone();
                match purpose {
                    PickerFor::Field(id) => self.editor_pick(&id, &value),
                    PickerFor::ConnectFrom(from) => self.editor_connect(&from, &value),
                    other => self.editor_settle_more(other, &value),
                }
            }
        }
    }

    /// Keys on an overlay: the prompts edit, the definition scrolls and
    /// closes.
    fn editor_overlay_key(&mut self, key: &KeyEvent) {
        let editor = self.editor();
        let Some(Overlay::Definition { scroll }) = editor.overlay.as_mut() else {
            self.editor_prompts_key(key);
            return;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('v') | KeyCode::Char('q') => editor.overlay = None,
            KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => *scroll += 1,
            KeyCode::PageUp => *scroll = scroll.saturating_sub(20),
            KeyCode::PageDown => *scroll += 20,
            KeyCode::Home => *scroll = 0,
            KeyCode::End => *scroll = usize::MAX / 2,
            KeyCode::Char('y') => {
                let text = editor.doc.to_toml();
                let copied = (self.yank_fn)(&text);
                self.editor().message = Some(if copied {
                    "Copied the definition".to_string()
                } else {
                    "Could not reach a clipboard".to_string()
                });
            }
            _ => {}
        }
    }

    /// `c`: connect the selected stage to another, picked from a list (or
    /// to itself, which the canvas cannot draw as a drag).
    fn editor_open_connect(&mut self) {
        let Some(from) = self.editor().panel_stage() else {
            self.editor().message = Some("Select a stage to connect from".to_string());
            return;
        };
        let existing: Vec<String> = self
            .editor()
            .doc
            .edges()
            .into_iter()
            .filter(|e| e.from == from)
            .map(|e| e.to)
            .collect();
        let rows: Vec<PickerOption> = self
            .editor()
            .doc
            .stage_names()
            .into_iter()
            .map(|name| PickerOption {
                detail: if name == from {
                    "itself: a loop, which needs max revisits".to_string()
                } else if existing.contains(&name) {
                    "already connected".to_string()
                } else {
                    String::new()
                },
                value: name,
            })
            .collect();
        let picker = Picker::new(
            format!("Connect {from} to"),
            vec![
                "A new path the model routes on; change what fires it in the inspector."
                    .to_string(),
            ],
            rows,
            0,
        );
        self.editor().picker = Some((PickerFor::ConnectFrom(from), picker));
    }

    /// Add the path, select it, and open its panel.
    pub(in crate::commands::dashboard) fn editor_connect(&mut self, from: &str, to: &str) {
        let (from, to) = (from.to_string(), to.to_string());
        if self.editor_mutate(|d| d.add_edge(&from, &to)) {
            if from == to {
                self.editor_open_self_loop(&from);
                return;
            }
            let editor = self.editor();
            editor.view.select_edge(&from, &to);
            editor.sync_panel();
            editor.focus = Focus::Inspector;
        }
    }

    /// `x`: delete what is selected on the canvas.
    fn editor_delete_selected(&mut self) {
        match self.editor().panel.clone() {
            Panel::Stage { name, .. } => self.editor_request_delete_stage(&name),
            Panel::Edge { from, to } => self.editor_delete_edge(&from, &to),
            Panel::Agent | Panel::External(_) | Panel::Region { .. } => {
                self.editor().message = Some("Select a stage or a path to delete".to_string());
            }
        }
    }

    /// After the canvas took the mouse: add the path a drag made, remember
    /// where a box was dropped, and keep the panel on the selection.
    pub(in crate::commands::dashboard) fn editor_drain_canvas(&mut self) {
        let Some(editor) = self.agents().editor.as_mut() else {
            return;
        };
        let events = editor.view.take_events();
        for event in events {
            match event {
                CanvasEvent::Connected { from, to } => self.editor_connect(&from, &to),
                CanvasEvent::Moved => {
                    let editor = self.editor();
                    let (name, positions) = (editor.name.clone(), editor.view.positions());
                    editor.layout.set(&name, positions);
                }
            }
        }
        self.editor().sync_panel();
    }
}
