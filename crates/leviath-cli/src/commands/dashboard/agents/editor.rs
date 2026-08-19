//! The agent editor's state: the document, the canvas, what is selected,
//! what is being edited, and the save/undo/problems machinery around them.

use std::path::PathBuf;
use std::sync::Arc;

use ratatui::text::Line;

use super::super::state::Dashboard;
use super::super::types::*;
use super::inspector::{self, Field, FieldId, FieldValue, Panel, StageTab};
use crate::blueprint_edit::check::{Problems, check};
use crate::blueprint_edit::{
    EdgeKind, EditError, LayoutStore, ManifestDoc, StageModeView, TransformKind, WorkerKind,
    catalog,
};
use crate::tui::flowgraph::{FlowView, Selection, StageGraph};
use crate::tui::widgets::confirm::Confirm;
use crate::tui::widgets::line_edit::LineEdit;
use crate::tui::widgets::picker::Picker;

/// Which pane the keys go to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::commands::dashboard) enum Focus {
    /// The graph: arrows move between boxes, `a`/`c`/`x` edit the graph.
    Canvas,
    /// The inspector: arrows move between fields, Enter edits one.
    Inspector,
}

/// What a chooser is choosing for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::commands::dashboard) enum PickerFor {
    /// A field's value, from the field's list.
    Field(FieldId),
    /// The other end of a new path from this stage.
    ConnectFrom(String),
    /// A model to append to the chain.
    AddModel,
    /// A model to put in place of the chain's `n`th.
    ReplaceModel(usize),
    /// The stage's tools (many).
    Tools,
    /// Which tool to route.
    RoutingTool,
    /// Where a tool's results land.
    RoutingRegion(String),
}

/// A full-screen overlay over the editor.
#[derive(Debug, Clone)]
pub(in crate::commands::dashboard) enum Overlay {
    /// The exact manifest that will be saved, scrolled by `scroll` lines.
    Definition { scroll: usize },
    /// A stage's prompts.
    Prompts(Box<super::prompts::PromptsEditor>),
}

/// What is being opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::commands::dashboard) enum EditTarget {
    /// An agent from the catalog: saved back where it lives (or installed,
    /// for a bundled one not installed yet).
    Existing {
        name: String,
        dir: Option<PathBuf>,
        bundled_from: Option<String>,
    },
    /// A new agent from a template, saved under the agents directory.
    New {
        name: String,
        bundled_from: Option<String>,
    },
}

/// The editor.
pub(in crate::commands::dashboard) struct Editor {
    /// The agent's name; the directory it saves to is named after it.
    pub(in crate::commands::dashboard) name: String,
    /// Not saved anywhere yet.
    pub(in crate::commands::dashboard) is_new: bool,
    /// The directory was made at open, only so the lint could see a
    /// bundled agent's scripts; closing without a save removes it again.
    pub(in crate::commands::dashboard) scratch_dir: bool,
    /// Where `agent.leviath` is written.
    pub(in crate::commands::dashboard) dir: PathBuf,
    pub(in crate::commands::dashboard) doc: ManifestDoc,
    /// The manifest before each edit, newest last.
    pub(in crate::commands::dashboard) undo: Vec<String>,
    pub(in crate::commands::dashboard) redo: Vec<String>,
    pub(in crate::commands::dashboard) view: FlowView,
    pub(in crate::commands::dashboard) graph: Arc<StageGraph>,
    pub(in crate::commands::dashboard) panel: Panel,
    pub(in crate::commands::dashboard) focus: Focus,
    /// The inspector's cursor, into `fields()`.
    pub(in crate::commands::dashboard) cursor: usize,
    /// A field being typed into.
    pub(in crate::commands::dashboard) line: Option<(FieldId, LineEdit)>,
    /// A chooser on top of everything.
    pub(in crate::commands::dashboard) picker: Option<(PickerFor, Picker)>,
    /// The name of a stage about to be added.
    pub(in crate::commands::dashboard) add_stage: Option<LineEdit>,
    /// The name of a region about to be added.
    pub(in crate::commands::dashboard) add_region: Option<LineEdit>,
    /// The canvas selection a pushed panel (a region, a stage's loop back
    /// to itself) was opened from: the panel stays while it holds.
    pub(in crate::commands::dashboard) panel_anchor: Option<Selection>,
    pub(in crate::commands::dashboard) overlay: Option<Overlay>,
    /// The right-click menu, while one is open.
    pub(in crate::commands::dashboard) menu: Option<super::context_menu::ContextMenu>,
    /// Where the next added stage goes on the canvas (a right click on empty
    /// canvas); `None` places it after the rightmost box.
    pub(in crate::commands::dashboard) place_next: Option<(f64, f64)>,
    pub(in crate::commands::dashboard) problems: Problems,
    /// The problems list expanded under the canvas.
    pub(in crate::commands::dashboard) problems_open: bool,
    pub(in crate::commands::dashboard) layout: LayoutStore,
    pub(in crate::commands::dashboard) dirty: bool,
    /// The last thing the editor said, on the status line.
    pub(in crate::commands::dashboard) message: Option<String>,
    /// `provider/model` ids the model chooser offers.
    pub(in crate::commands::dashboard) models: Vec<String>,
    /// Tool names the tools chooser offers.
    pub(in crate::commands::dashboard) tools: Vec<String>,
    /// Where the last frame put the inspector's rows: the screen row of
    /// each field, and the column span of each stage tab, so a click lands
    /// on the right one.
    pub(in crate::commands::dashboard) hit: InspectorHits,
}

/// What the inspector drew last, for the mouse.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::commands::dashboard) struct InspectorHits {
    /// The inspector's area (border included).
    pub(in crate::commands::dashboard) area: ratatui::layout::Rect,
    /// The screen row each field sits on, in field order.
    pub(in crate::commands::dashboard) rows: Vec<u16>,
    /// The screen row of the stage tab bar, and each tab's `[x0, x1)`.
    pub(in crate::commands::dashboard) tabs: Option<(u16, Vec<(u16, u16)>)>,
}

/// Snapshots kept for undo.
const UNDO_DEPTH: usize = 100;

/// An edit to run on the document.
type Mutation<'a> = Box<dyn FnOnce(&mut ManifestDoc) -> Result<(), EditError> + 'a>;

/// A linear chain of `names`, for a manifest the runtime cannot read yet.
fn chain_graph(names: &[String]) -> Arc<StageGraph> {
    let mut text = String::from("[agent]\nname = \"chain\"\n");
    for name in names {
        text.push_str(&format!("[stages.{name}]\n"));
    }
    let bp = leviath_core::manifest::parse_manifest(&text)
        .expect("stage names passed the manifest's charset");
    Arc::new(StageGraph::from_blueprint(&bp))
}

impl Editor {
    /// The graph the runtime would run; when the runtime rejects the
    /// manifest, the last graph that parsed, or a bare chain of the stage
    /// names so there is still something to point at.
    fn graph_of(doc: &ManifestDoc, fallback: Option<&Arc<StageGraph>>) -> Arc<StageGraph> {
        match doc.blueprint() {
            Ok(bp) => Arc::new(StageGraph::from_blueprint(&bp)),
            Err(_) => fallback
                .cloned()
                .unwrap_or_else(|| chain_graph(&doc.stage_names())),
        }
    }

    /// The inspector's rows for the current panel.
    pub(in crate::commands::dashboard) fn fields(&self) -> Vec<Field> {
        inspector::fields(&self.doc, &self.panel)
    }

    /// The row under the inspector cursor.
    pub(in crate::commands::dashboard) fn current_field(&self) -> Option<Field> {
        self.fields().into_iter().nth(self.cursor)
    }

    /// Bring the panel in line with the canvas selection, keeping a stage
    /// panel's tab across stages.
    pub(in crate::commands::dashboard) fn sync_panel(&mut self) {
        // A pushed panel (a region, a loop back to the same stage) stays
        // while the selection that opened it holds and what it shows exists.
        let pushed = match &self.panel {
            Panel::Region { scope, name, .. } => self.doc.region(scope.stage(), name).is_some(),
            Panel::Edge { from, to } if from == to => self.doc.edge(from, to).is_some(),
            _ => false,
        };
        if pushed && self.panel_anchor.as_ref() == Some(&self.view.selection()) {
            return;
        }
        self.panel_anchor = None;
        let tab = match &self.panel {
            Panel::Stage { tab, .. } => *tab,
            _ => StageTab::Behaviour,
        };
        let next = match self.view.selection() {
            Selection::Node(id) => match id.strip_prefix("ext:") {
                Some(name) => Panel::External(name.to_string()),
                None => Panel::Stage { name: id, tab },
            },
            Selection::Edge(edge) => Panel::Edge {
                from: edge.from,
                to: edge.to,
            },
            Selection::Nothing => Panel::Agent,
        };
        if next != self.panel {
            self.panel = next;
            self.cursor = 0;
        }
        let count = self.fields().len();
        self.cursor = self.cursor.min(count.saturating_sub(1));
    }

    /// Re-read everything derived from the document after an edit: the
    /// graph and canvas, the problems, the badges.
    pub(in crate::commands::dashboard) fn refresh(&mut self) {
        self.graph = Self::graph_of(&self.doc, Some(&self.graph));
        let positions = self.view.positions();
        self.view.replace_graph(self.graph.clone(), positions);
        self.problems = check(&self.doc.to_toml(), &self.dir);
        self.apply_flags();
        self.sync_panel();
    }

    /// The `!` and `▣` badges on the boxes. Only an error earns the `!`: a
    /// warning on every new stage (no model yet) would mark every box.
    fn apply_flags(&mut self) {
        for stage in self.doc.stages() {
            let problem = self
                .problems
                .for_stage(&stage.name)
                .iter()
                .any(|p| p.severity == crate::blueprint_edit::check::Severity::Error);
            self.view
                .set_flags(&stage.name, problem, stage.has_own_layout);
        }
    }

    /// Run a mutator: the manifest before it goes on the undo stack, and a
    /// refusal comes back as its message with nothing changed. Tests drive
    /// the document through here; the dashboard goes through
    /// [`Dashboard::editor_mutate`].
    #[cfg(test)]
    pub(in crate::commands::dashboard) fn mutate(
        &mut self,
        f: impl FnOnce(&mut ManifestDoc) -> Result<(), EditError>,
    ) -> Result<(), String> {
        self.mutate_boxed(Box::new(f))
    }

    /// One function whatever closure the caller passes, so coverage sees its
    /// branches once rather than per call site.
    fn mutate_boxed(&mut self, f: Mutation<'_>) -> Result<(), String> {
        let before = self.doc.to_toml();
        match f(&mut self.doc) {
            Ok(()) => {
                if self.doc.to_toml() != before {
                    self.undo.push(before);
                    if self.undo.len() > UNDO_DEPTH {
                        self.undo.remove(0);
                    }
                    self.redo.clear();
                    self.dirty = true;
                    self.refresh();
                }
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// Back one edit.
    pub(in crate::commands::dashboard) fn undo(&mut self) -> bool {
        let Some(text) = self.undo.pop() else {
            return false;
        };
        self.redo.push(self.doc.to_toml());
        self.doc = ManifestDoc::parse(&text).expect("an undo snapshot parsed once");
        self.dirty = true;
        self.refresh();
        true
    }

    /// Forward one edit.
    pub(in crate::commands::dashboard) fn redo(&mut self) -> bool {
        let Some(text) = self.redo.pop() else {
            return false;
        };
        self.undo.push(self.doc.to_toml());
        self.doc = ManifestDoc::parse(&text).expect("a redo snapshot parsed once");
        self.dirty = true;
        self.refresh();
        true
    }

    /// The name of the stage the panel is on, when it is on one.
    pub(in crate::commands::dashboard) fn panel_stage(&self) -> Option<String> {
        match &self.panel {
            Panel::Stage { name, .. } => Some(name.clone()),
            _ => None,
        }
    }

    /// The path the panel is on, when it is on one.
    pub(in crate::commands::dashboard) fn panel_edge(&self) -> Option<(String, String)> {
        match &self.panel {
            Panel::Edge { from, to } => Some((from.clone(), to.clone())),
            _ => None,
        }
    }
}

impl Dashboard {
    /// Open the editor on `text`.
    pub(in crate::commands::dashboard) fn open_editor(&mut self, target: EditTarget, text: &str) {
        let doc = match ManifestDoc::parse(text) {
            Ok(doc) => doc,
            Err(e) => {
                self.toast(format!("Cannot edit that manifest: {e}"), ToastLevel::Error);
                return;
            }
        };
        let (name, is_new, dir, bundled_from) = match target {
            EditTarget::Existing {
                name,
                dir,
                bundled_from,
            } => {
                let dir = dir.unwrap_or_else(|| self.new_run_ctx.agents_dir.join(&name));
                (name, false, dir, bundled_from)
            }
            EditTarget::New { name, bundled_from } => {
                let dir = self.new_run_ctx.agents_dir.join(&name);
                (name, true, dir, bundled_from)
            }
        };
        // A bundled agent not installed yet (edited as itself, or as the
        // start of a new one) brings its scripts along now, so the lint sees
        // the tools they define and a save leaves a complete install; the
        // directory is removed again when the editor closes without saving.
        let mut scratch_dir = false;
        if !dir.exists()
            && let Some(from) = bundled_from.as_deref().and_then(catalog::bundled)
        {
            let _ = catalog::copy_bundled_extras(&self.new_run_ctx.agents_dir, &name, from);
            scratch_dir = true;
        }
        let layout = self.layout_store();
        let positions = layout.positions(&name).cloned().unwrap_or_default();
        let graph = Editor::graph_of(&doc, None);
        let view = FlowView::new_editor(graph.clone(), positions);
        let problems = check(text, &dir);
        let mut models: Vec<String> = crate::commands::models::closed_catalog_models()
            .into_iter()
            .map(|(p, m)| format!("{p}/{m}"))
            .collect();
        models.extend(doc.known_models());
        models.extend(
            self.agent_builder
                .as_deref()
                .map(|s| s.model_catalog.clone())
                .unwrap_or_default(),
        );
        models.sort();
        models.dedup();
        // The tools this install has: built in, plus scripts under the
        // agent's directory and the ones the manifest already names.
        let mut tools: Vec<String> =
            crate::tool_inventory::ToolInventory::discover(Some(&dir), Some(&name))
                .names()
                .into_iter()
                .collect();
        tools.extend(doc.known_tools());
        tools.sort();
        tools.dedup();
        let mut editor = Editor {
            name,
            is_new,
            scratch_dir,
            dir,
            doc,
            undo: Vec::new(),
            redo: Vec::new(),
            view,
            graph,
            panel: Panel::Agent,
            focus: Focus::Canvas,
            cursor: 0,
            line: None,
            picker: None,
            add_stage: None,
            add_region: None,
            panel_anchor: None,
            overlay: None,
            menu: None,
            place_next: None,
            problems,
            problems_open: false,
            layout,
            dirty: is_new,
            message: None,
            models,
            tools,
            hit: InspectorHits::default(),
        };
        editor.apply_flags();
        self.agents().editor = Some(editor);
    }

    /// The open editor.
    pub(in crate::commands::dashboard) fn editor(&mut self) -> &mut Editor {
        self.agents()
            .editor
            .as_mut()
            .expect("callers check the editor is open")
    }

    /// Run a mutator on the open editor and say when it was refused.
    pub(in crate::commands::dashboard) fn editor_mutate(
        &mut self,
        f: impl FnOnce(&mut ManifestDoc) -> Result<(), EditError>,
    ) -> bool {
        self.editor_mutate_boxed(Box::new(f))
    }

    fn editor_mutate_boxed(&mut self, f: Mutation<'_>) -> bool {
        match self.editor().mutate_boxed(f) {
            Ok(()) => true,
            Err(message) => {
                self.editor().message = Some(message);
                false
            }
        }
    }

    /// Ctrl-S: check, then write. Errors block the save and open the
    /// problems list, so the reason is on screen.
    pub(in crate::commands::dashboard) fn editor_save(&mut self) {
        let text = self.editor().doc.to_toml();
        let dir = self.editor().dir.clone();
        let problems = check(&text, &dir);
        self.editor().problems = problems.clone();
        if !problems.is_saveable() {
            self.editor().problems_open = true;
            let n = problems.error_count();
            self.editor().message = Some(format!(
                "Not saved: {n} problem{} to fix first",
                if n == 1 { "" } else { "s" }
            ));
            return;
        }
        let name = self.editor().name.clone();
        let written = std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::write(dir.join("agent.leviath"), &text));
        if let Err(e) = written {
            self.editor().message = Some(format!("Could not write {}: {e}", dir.display()));
            return;
        }
        let positions = self.editor().view.positions();
        {
            let editor = self.editor();
            editor.layout.set(&name, positions);
            let _ = editor.layout.save();
            editor.dirty = false;
            editor.is_new = false;
            editor.message = Some(format!("Saved to {}", dir.join("agent.leviath").display()));
        }
        self.refresh_catalog();
        self.toast(format!("Saved {name}"), ToastLevel::Info);
    }

    /// Esc on the canvas: close, asking first when there are unsaved edits.
    pub(in crate::commands::dashboard) fn editor_close_requested(&mut self) {
        if self.editor().dirty {
            let dialog = Confirm::new(
                "Discard your changes?",
                vec![Line::from(
                    "The agent has unsaved edits. Close the editor and lose them?",
                )],
                "Discard",
                "Keep editing",
            )
            .danger();
            self.pending_confirm = Some((ConfirmAction::EditorDiscard, dialog));
            return;
        }
        self.close_editor();
    }

    /// Close the editor, back to the catalog.
    pub(in crate::commands::dashboard) fn close_editor(&mut self) {
        // The arrangement is worth keeping even when the manifest is not.
        let (name, positions, unsaved_dir) = {
            let editor = self.editor();
            (
                editor.name.clone(),
                editor.view.positions(),
                editor.scratch_dir.then(|| editor.dir.clone()),
            )
        };
        {
            let editor = self.editor();
            editor.layout.set(&name, positions);
            let _ = editor.layout.save();
        }
        // An agent never saved leaves nothing behind (its scripts were
        // materialised for the lint's sake).
        if let Some(dir) = unsaved_dir
            && !dir.join("agent.leviath").exists()
        {
            let _ = std::fs::remove_dir_all(&dir);
        }
        self.agents().editor = None;
        self.refresh_catalog();
    }

    /// Enter on an inspector row.
    pub(in crate::commands::dashboard) fn editor_activate(&mut self) {
        let Some(field) = self.editor().current_field() else {
            return;
        };
        if !field.enabled {
            return;
        }
        match field.value {
            FieldValue::Text(text) => {
                self.editor().line = Some((field.id, LineEdit::new(text, false)));
            }
            FieldValue::Number(n) => {
                let text = n.map(|n| n.to_string()).unwrap_or_default();
                self.editor().line = Some((field.id, LineEdit::new(text, false)));
            }
            FieldValue::Toggle(on) => self.editor_set_toggle(&field.id, !on),
            FieldValue::Choice(_) => self.editor_open_choice(&field.id),
            FieldValue::Row(_) => self.editor_open_row(&field.id),
            FieldValue::Button => self.editor_button(&field.id),
            FieldValue::Segment { .. } => self.editor_cycle_segment(&field.id, 1),
        }
    }

    /// `←`/`→` on an inspector row: cycle a choice, step a number, flip a
    /// toggle.
    pub(in crate::commands::dashboard) fn editor_adjust(&mut self, delta: isize) {
        let Some(field) = self.editor().current_field() else {
            return;
        };
        if !field.enabled {
            return;
        }
        match field.value {
            FieldValue::Toggle(on) => self.editor_set_toggle(&field.id, !on),
            FieldValue::Number(n) => {
                let next = match (n, delta) {
                    (None, d) if d > 0 => Some(1),
                    (None, _) => None,
                    (Some(0), d) if d < 0 => None,
                    (Some(v), d) => Some((v as i64 + d as i64).max(0) as u64),
                };
                self.editor_set_number(&field.id, next);
            }
            FieldValue::Choice(_) => {
                let (options, current) = self.editor_choice_options(&field.id);
                if options.is_empty() {
                    return;
                }
                let at = current.unwrap_or(0) as isize + delta;
                let at = at.rem_euclid(options.len() as isize) as usize;
                self.editor_pick(&field.id, &options[at]);
            }
            FieldValue::Segment { .. } => self.editor_cycle_segment(&field.id, delta),
            FieldValue::Row(_) => {
                if let FieldId::ModelEntry(i) = field.id {
                    self.editor_move_model(i, delta);
                }
            }
            FieldValue::Text(_) | FieldValue::Button => {}
        }
    }

    pub(in crate::commands::dashboard) fn editor_set_toggle(&mut self, id: &FieldId, on: bool) {
        match id {
            FieldId::AllowComplete => {
                let stage = self.editor().panel_stage().expect("a stage field");
                self.editor_mutate(|d| d.set_allow_complete(&stage, on.then_some(true)));
            }
            FieldId::EdgeGate => {
                let (from, to) = self.editor().panel_edge().expect("a path field");
                self.editor_mutate(|d| d.set_edge_gate(&from, &to, on));
            }
            _ => self.editor_set_toggle_more(id, on),
        }
    }

    pub(in crate::commands::dashboard) fn editor_set_number(
        &mut self,
        id: &FieldId,
        value: Option<u64>,
    ) {
        if self.editor_set_number_more(id, value) {
            return;
        }
        let stage = self.editor().panel_stage().expect("a stage field");
        match id {
            FieldId::MaxIterations => {
                self.editor_mutate(|d| d.set_max_iterations(&stage, value));
            }
            FieldId::MaxRevisits => {
                self.editor_mutate(|d| d.set_max_revisits(&stage, value));
            }
            FieldId::MaxWorkers => {
                self.editor_mutate(|d| {
                    d.set_fan_out(
                        &stage,
                        crate::blueprint_edit::FanOutField::MaxWorkers(value),
                    )
                });
            }
            FieldId::MaxItems => {
                self.editor_mutate(|d| {
                    d.set_fan_out(&stage, crate::blueprint_edit::FanOutField::MaxItems(value))
                });
            }
            _ => {}
        }
    }

    /// A field's choices, and which of them is current.
    pub(in crate::commands::dashboard) fn editor_choice_options(
        &mut self,
        id: &FieldId,
    ) -> (Vec<String>, Option<usize>) {
        let editor = self.editor();
        match id {
            FieldId::EntryStage => {
                let names = editor.doc.stage_names();
                let current = editor
                    .doc
                    .agent()
                    .entry_stage
                    .and_then(|e| names.iter().position(|n| *n == e));
                (names, current)
            }
            FieldId::DefaultModel => {
                let current = editor
                    .doc
                    .agent()
                    .default_model
                    .and_then(|m| editor.models.iter().position(|n| *n == m));
                (editor.models.clone(), current)
            }
            FieldId::StageMode => {
                let stage = editor.panel_stage().expect("a stage field");
                let mode = editor.doc.stage(&stage).map(|s| s.mode);
                let options: Vec<String> = StageModeView::CHOICES
                    .iter()
                    .map(|m| m.as_str().to_string())
                    .collect();
                let current = mode.and_then(|m| options.iter().position(|o| *o == m.as_str()));
                (options, current)
            }
            FieldId::WorkerKind => {
                let stage = editor.panel_stage().expect("a stage field");
                let kind = editor
                    .doc
                    .stage(&stage)
                    .and_then(|s| s.fan_out.worker.map(|(k, _)| k));
                let options: Vec<String> =
                    [WorkerKind::Stage, WorkerKind::Agent, WorkerKind::Query]
                        .iter()
                        .map(|k| k.key().to_string())
                        .collect();
                let current = kind.and_then(|k| options.iter().position(|o| *o == k.key()));
                (options, current)
            }
            FieldId::MergeStage => {
                let stage = editor.panel_stage().expect("a stage field");
                let mut names = vec!["(none)".to_string()];
                names.extend(editor.doc.stage_names().into_iter().filter(|n| *n != stage));
                let merge = editor.doc.stage(&stage).and_then(|s| s.fan_out.merge_stage);
                let current = merge
                    .and_then(|m| names.iter().position(|n| *n == m))
                    .or(Some(0));
                (names, current)
            }
            FieldId::OnWorkerFailure => {
                let stage = editor.panel_stage().expect("a stage field");
                let options = vec!["continue".to_string(), "fail_all".to_string()];
                let policy = editor
                    .doc
                    .stage(&stage)
                    .and_then(|s| s.fan_out.on_worker_failure);
                let current = policy
                    .and_then(|p| options.iter().position(|o| *o == p))
                    .or(Some(0));
                (options, current)
            }
            FieldId::EdgeKind => {
                let (from, to) = editor.panel_edge().expect("a path field");
                let options: Vec<String> = EdgeKind::CHOICES
                    .iter()
                    .map(|k| k.label().to_string())
                    .collect();
                let current = editor
                    .doc
                    .edge(&from, &to)
                    .and_then(|e| EdgeKind::CHOICES.iter().position(|k| *k == e.kind));
                (options, current)
            }
            _ => self.editor_choice_options_more(id),
        }
    }

    /// Open the chooser for a choice field.
    pub(in crate::commands::dashboard) fn editor_open_choice(&mut self, id: &FieldId) {
        let (options, current) = self.editor_choice_options(id);
        if options.is_empty() {
            return;
        }
        let (title, explain, details): (&str, &str, Vec<String>) = match id {
            FieldId::RoutingDefault => (
                "Tool results land in",
                "Where a tool's output goes unless a row says otherwise.",
                vec![],
            ),
            FieldId::EdgeTransform => (
                "Context carried over",
                "What crosses the path with the run. Pinned regions always do.",
                TransformKind::CHOICES
                    .iter()
                    .map(|t| t.label().to_string())
                    .collect(),
            ),
            FieldId::RegionKind => (
                "How the region behaves",
                "What the runtime does with it as the window fills.",
                inspector::REGION_KINDS
                    .iter()
                    .map(|(_, h)| h.to_string())
                    .collect(),
            ),
            FieldId::EntryStage => ("Starts at", "The stage a run begins in.", vec![]),
            FieldId::DefaultModel => (
                "Default model",
                "The model every stage tries first; the rest of each stage's chain stays behind it.",
                vec![],
            ),
            FieldId::StageMode => (
                "How it works",
                "What the stage does with its turn.",
                StageModeView::CHOICES
                    .iter()
                    .map(|m| m.label().to_string())
                    .collect(),
            ),
            FieldId::WorkerKind => (
                "Workers come from",
                "Where a fan-out stage gets its workers.",
                [WorkerKind::Stage, WorkerKind::Agent, WorkerKind::Query]
                    .iter()
                    .map(|k| inspector::worker_kind_label(*k).to_string())
                    .collect(),
            ),
            FieldId::MergeStage => (
                "Merge stage",
                "The stage that gathers the workers' results.",
                vec![],
            ),
            FieldId::OnWorkerFailure => (
                "If a worker fails",
                "Carry on with the rest, or fail the whole fan-out.",
                vec![],
            ),
            _ => (
                "When to take this path",
                "What makes the run follow this path out of the stage.",
                EdgeKind::CHOICES
                    .iter()
                    .map(|k| k.short().to_string())
                    .collect(),
            ),
        };
        let rows: Vec<crate::tui::widgets::picker::PickerOption> = options
            .iter()
            .enumerate()
            .map(|(i, value)| crate::tui::widgets::picker::PickerOption {
                value: value.clone(),
                detail: details.get(i).cloned().unwrap_or_default(),
            })
            .collect();
        let picker = Picker::new(title, vec![explain.to_string()], rows, current.unwrap_or(0));
        self.editor().picker = Some((PickerFor::Field(id.clone()), picker));
    }

    /// Write a chosen value into a choice field.
    pub(in crate::commands::dashboard) fn editor_pick(&mut self, id: &FieldId, value: &str) {
        let value = value.to_string();
        match id {
            FieldId::EntryStage => {
                self.editor_mutate(|d| d.set_entry_stage(&value));
            }
            FieldId::DefaultModel => {
                self.editor_mutate(|d| {
                    d.set_default_model(&value);
                    Ok(())
                });
            }
            FieldId::StageMode => {
                let stage = self.editor().panel_stage().expect("a stage field");
                let mode = StageModeView::parse(&value);
                self.editor_mutate(|d| d.set_stage_mode(&stage, &mode));
            }
            FieldId::WorkerKind => {
                let stage = self.editor().panel_stage().expect("a stage field");
                let kind = [WorkerKind::Stage, WorkerKind::Agent, WorkerKind::Query]
                    .into_iter()
                    .find(|k| k.key() == value)
                    .expect("an offered kind");
                let current = self
                    .editor()
                    .doc
                    .stage(&stage)
                    .and_then(|s| s.fan_out.worker.map(|(_, v)| v))
                    .unwrap_or_default();
                self.editor_mutate(|d| {
                    d.set_fan_out(
                        &stage,
                        crate::blueprint_edit::FanOutField::Worker(Some((kind, current))),
                    )
                });
            }
            FieldId::MergeStage => {
                let stage = self.editor().panel_stage().expect("a stage field");
                let merge = (value != "(none)").then_some(value);
                self.editor_mutate(|d| {
                    d.set_fan_out(
                        &stage,
                        crate::blueprint_edit::FanOutField::MergeStage(merge),
                    )
                });
            }
            FieldId::OnWorkerFailure => {
                let stage = self.editor().panel_stage().expect("a stage field");
                self.editor_mutate(|d| {
                    d.set_fan_out(
                        &stage,
                        crate::blueprint_edit::FanOutField::OnWorkerFailure(Some(value)),
                    )
                });
            }
            FieldId::EdgeKind => {
                let (from, to) = self.editor().panel_edge().expect("a path field");
                let kind = EdgeKind::CHOICES
                    .into_iter()
                    .find(|k| k.label() == value)
                    .expect("an offered kind");
                self.editor_mutate(|d| d.set_edge_kind(&from, &to, kind));
            }
            _ => self.editor_pick_more(id, &value),
        }
    }

    /// Enter on a button row.
    pub(in crate::commands::dashboard) fn editor_button(&mut self, id: &FieldId) {
        match id {
            FieldId::MoveUp | FieldId::MoveDown => {
                let stage = self.editor().panel_stage().expect("a stage field");
                let up = *id == FieldId::MoveUp;
                self.editor_mutate(|d| d.move_stage(&stage, up));
            }
            FieldId::DeleteStage => {
                let stage = self.editor().panel_stage().expect("a stage field");
                self.editor_request_delete_stage(&stage);
            }
            FieldId::DeletePath => {
                let (from, to) = self.editor().panel_edge().expect("a path field");
                self.editor_delete_edge(&from, &to);
            }
            _ => self.editor_button_more(id),
        }
    }

    /// Ask before a stage goes: every path in and out goes with it.
    pub(in crate::commands::dashboard) fn editor_request_delete_stage(&mut self, stage: &str) {
        if self.editor().doc.stage_names().len() < 2 {
            self.editor().message =
                Some("This is the only stage; an agent needs at least one.".to_string());
            return;
        }
        let dialog = Confirm::new(
            "Delete stage?",
            vec![Line::from(format!(
                "Delete '{stage}' and every path in or out of it?"
            ))],
            "Delete",
            "Cancel",
        )
        .danger();
        self.pending_confirm = Some((
            ConfirmAction::StageDelete {
                name: stage.to_string(),
            },
            dialog,
        ));
    }

    /// The confirmed stage delete.
    pub(in crate::commands::dashboard) fn editor_delete_stage(&mut self, stage: &str) {
        let stage = stage.to_string();
        if self.editor_mutate(|d| d.delete_stage(&stage)) {
            self.editor().view.clear_selection();
            self.editor().sync_panel();
        }
    }

    /// Delete a path (no confirmation: The Lair asks none, and undo is one
    /// key away).
    pub(in crate::commands::dashboard) fn editor_delete_edge(&mut self, from: &str, to: &str) {
        let (from, to) = (from.to_string(), to.to_string());
        if self.editor_mutate(|d| d.delete_edge(&from, &to)) {
            self.editor().view.select_stage(&from);
            self.editor().sync_panel();
        }
    }

    /// The text of a line editor, committed to its field.
    pub(in crate::commands::dashboard) fn editor_commit_line(&mut self, id: &FieldId, text: &str) {
        let text = text.trim().to_string();
        match id {
            FieldId::AgentDescription => {
                self.editor_mutate(|d| {
                    d.set_description(&text);
                    Ok(())
                });
            }
            FieldId::StageName => {
                let stage = self.editor().panel_stage().expect("a stage field");
                // The box keeps its place under the new name.
                let positions = self
                    .editor()
                    .view
                    .positions()
                    .into_iter()
                    .map(|(id, at)| (if id == stage { text.clone() } else { id }, at))
                    .collect();
                if self.editor_mutate(|d| d.rename_stage(&stage, &text)) {
                    let editor = self.editor();
                    let graph = editor.graph.clone();
                    editor.view.replace_graph(graph, positions);
                    editor.view.select_stage(&text);
                    editor.sync_panel();
                }
            }
            FieldId::StageDescription => {
                let stage = self.editor().panel_stage().expect("a stage field");
                self.editor_mutate(|d| {
                    d.set_stage_text(&stage, crate::blueprint_edit::StageText::Description, &text)
                });
            }
            FieldId::MaxIterations
            | FieldId::MaxRevisits
            | FieldId::MaxWorkers
            | FieldId::MaxItems
            | FieldId::RegionBudget
            | FieldId::RegionMaxTokens
            | FieldId::RegionMinTokens
            | FieldId::RegionMaxItems
            | FieldId::RegionOverflow => {
                let value = match text.parse::<u64>() {
                    Ok(n) => Some(n),
                    Err(_) if text.is_empty() => None,
                    Err(_) => {
                        self.editor().message = Some(format!("\"{text}\" is not a whole number"));
                        return;
                    }
                };
                self.editor_set_number(id, value);
            }
            FieldId::WorkerRef => {
                let stage = self.editor().panel_stage().expect("a stage field");
                let kind = self
                    .editor()
                    .doc
                    .stage(&stage)
                    .and_then(|s| s.fan_out.worker.map(|(k, _)| k))
                    .unwrap_or(WorkerKind::Stage);
                let worker = (!text.is_empty()).then_some((kind, text));
                self.editor_mutate(|d| {
                    d.set_fan_out(&stage, crate::blueprint_edit::FanOutField::Worker(worker))
                });
            }
            FieldId::EdgeHint => {
                let (from, to) = self.editor().panel_edge().expect("a path field");
                self.editor_mutate(|d| d.set_edge_hint(&from, &to, &text));
            }
            _ => self.editor_commit_line_more(id, &text),
        }
    }
}
