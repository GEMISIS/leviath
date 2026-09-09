//! The inspector's other panels: a stage's model chain and tools, its
//! context layout and tool routing, a region, and a path's transform. The
//! core (`editor.rs`) hands a field it does not know to the `*_more`
//! functions here.

use std::collections::BTreeSet;

use super::super::state::Dashboard;
use super::super::types::ConfirmAction;
use super::editor::{ModalBase, PickerFor, TYPE_ANOTHER};
use super::inspector::{FieldId, Panel, REGION_KINDS};
use crate::blueprint_edit::{
    ArtifactField, InputList, RegionField, RegionScope, RegionValue, Rule, TransformKind,
    split_list,
};
use crate::tui::widgets::confirm::Confirm;
use crate::tui::widgets::line_edit::LineEdit;
use crate::tui::widgets::picker::{Picker, PickerOption};
use ratatui::text::Line;

/// A word for a chooser row: what a family or a wildcard stands for.
fn type_detail(value: &str) -> String {
    match value.split_once('/') {
        Some(("*", "*")) => "anything".to_string(),
        Some((kind, "*")) => format!("every {kind} type"),
        _ => String::new(),
    }
}

/// `200k`, `1M`, `1.5M`: a context window as the picker shows it.
pub(super) fn window_label(tokens: usize) -> String {
    if tokens >= 1_000_000 {
        let m = tokens as f64 / 1_000_000.0;
        let text = format!("{m:.1}");
        format!("{}M", text.trim_end_matches(".0"))
    } else {
        format!("{}k", tokens / 1000)
    }
}

/// The regions a stage's tool routing may name: its effective layout plus
/// the regions the runtime always provides.
const ALWAYS_VISIBLE: [&str; 4] = [
    "conversation",
    "tool_results",
    "final_output",
    "stage_instructions",
];

impl Dashboard {
    /// The scope of the region panel, when the inspector is on one.
    fn panel_region(&mut self) -> Option<(RegionScope, String)> {
        match &self.editor().panel {
            Panel::Region { scope, name } => Some((scope.clone(), name.clone())),
            _ => None,
        }
    }

    /// The declared file a window is open on, when one is.
    fn panel_artifact(&mut self) -> Option<(String, usize)> {
        match &self.editor().panel {
            Panel::Artifact { stage, index } => Some((stage.clone(), *index)),
            _ => None,
        }
    }

    /// A toggle the core does not know: the region's `required`, an
    /// artifact's.
    pub(super) fn editor_set_toggle_more(&mut self, id: &FieldId, on: bool) {
        match id {
            FieldId::RegionRequired => {
                if let Some((scope, name)) = self.panel_region() {
                    self.editor_mutate(|d| {
                        d.set_region_field(
                            &scope,
                            &name,
                            RegionField::Required,
                            RegionValue::Flag(on),
                        )
                    });
                }
            }
            FieldId::ArtifactRequired => {
                if let Some((stage, i)) = self.panel_artifact() {
                    self.editor_mutate(|d| d.set_artifact(&stage, i, ArtifactField::Required(on)));
                }
            }
            _ => {}
        }
    }

    /// A number the core does not know: the region's sizes.
    pub(super) fn editor_set_number_more(&mut self, id: &FieldId, value: Option<u64>) -> bool {
        let field = match id {
            FieldId::RegionBudget => RegionField::BudgetPercent,
            FieldId::RegionMaxTokens => RegionField::MaxTokens,
            FieldId::RegionMinTokens => RegionField::MinTokens,
            FieldId::RegionMaxItems => RegionField::MaxItems,
            FieldId::RegionOverflow => RegionField::Overflow,
            FieldId::RegionMaxStored => RegionField::MaxStored,
            _ => return false,
        };
        if let Some((scope, name)) = self.panel_region() {
            self.editor_mutate(|d| {
                d.set_region_field(&scope, &name, field, RegionValue::Number(value))
            });
        }
        true
    }

    /// Choices the core does not know: the routing default, the transform,
    /// the region kind.
    pub(super) fn editor_choice_options_more(
        &mut self,
        id: &FieldId,
    ) -> (Vec<String>, Option<usize>) {
        match id {
            FieldId::RoutingDefault => {
                let stage = self.editor().panel_stage().expect("a stage field");
                let options = self.routing_regions(&stage);
                let current = self
                    .editor()
                    .doc
                    .tool_routing(&stage)
                    .default_region
                    .and_then(|r| options.iter().position(|o| *o == r))
                    .or(Some(0));
                (options, current)
            }
            FieldId::EdgeTransform => {
                let (from, to) = self.editor().panel_edge().expect("a path field");
                let options: Vec<String> = TransformKind::CHOICES
                    .iter()
                    .map(|t| t.as_str().to_string())
                    .collect();
                let current = self.editor().doc.edge(&from, &to).and_then(|e| {
                    TransformKind::CHOICES
                        .iter()
                        .position(|t| *t == e.transform)
                });
                (options, current)
            }
            FieldId::RegionKind => {
                let options: Vec<String> =
                    REGION_KINDS.iter().map(|(k, _)| k.to_string()).collect();
                let current = self
                    .panel_region()
                    .and_then(|(scope, name)| self.editor().doc.region(scope.stage(), &name))
                    .and_then(|r| options.iter().position(|o| *o == r.kind));
                (options, current)
            }
            _ => (Vec::new(), None),
        }
    }

    /// `(default)` then every region the stage's routing may name.
    fn routing_regions(&mut self, stage: &str) -> Vec<String> {
        let mut options = vec!["(default)".to_string()];
        options.extend(
            self.editor()
                .doc
                .effective_regions(Some(stage))
                .regions
                .into_iter()
                .map(|r| r.name),
        );
        for always in ALWAYS_VISIBLE {
            if !options.iter().any(|o| o == always) {
                options.push(always.to_string());
            }
        }
        options
    }

    /// A pick the core does not know.
    pub(super) fn editor_pick_more(&mut self, id: &FieldId, value: &str) {
        let value = value.to_string();
        match id {
            FieldId::RoutingDefault => {
                let stage = self.editor().panel_stage().expect("a stage field");
                let region = if value == "(default)" {
                    String::new()
                } else {
                    value
                };
                self.editor_mutate(|d| d.set_tool_routing_default(&stage, &region));
            }
            FieldId::EdgeTransform => {
                let (from, to) = self.editor().panel_edge().expect("a path field");
                let kind = TransformKind::parse(&value);
                self.editor_mutate(|d| d.set_transform(&from, &to, &kind));
            }
            FieldId::RegionKind => {
                if let Some((scope, name)) = self.panel_region() {
                    self.editor_mutate(|d| {
                        d.set_region_field(
                            &scope,
                            &name,
                            RegionField::Kind,
                            RegionValue::Text(value),
                        )
                    });
                }
            }
            _ => {}
        }
    }

    /// A button the core does not know.
    pub(super) fn editor_button_more(&mut self, id: &FieldId) {
        match id {
            FieldId::EditPrompts => self.editor_open_prompts(),
            FieldId::AddModel => self.editor_open_model_picker(PickerFor::AddModel),
            FieldId::AddRegion => {
                self.editor().add_region = Some(LineEdit::new(String::new(), false));
            }
            FieldId::OwnLayout => {
                let stage = self.editor().panel_stage().expect("a stage field");
                if self.editor().doc.effective_regions(Some(&stage)).inherited {
                    self.editor_mutate(|d| d.create_stage_override(&stage));
                } else {
                    let dialog = Confirm::new(
                        "Remove its own layout?",
                        vec![Line::from(format!(
                            "Drop {stage}'s own context regions and go back to the shared layout?"
                        ))],
                        "Remove",
                        "Keep",
                    )
                    .danger();
                    self.pending_confirm = Some((ConfirmAction::OverrideRemove { stage }, dialog));
                }
            }
            FieldId::AddRouting => {
                let stage = self.editor().panel_stage().expect("a stage field");
                let routed = self.editor().doc.tool_routing(&stage);
                let tools: Vec<String> = self
                    .editor()
                    .doc
                    .stage(&stage)
                    .map(|s| s.tools)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|t| !routed.overrides.iter().any(|(r, _)| r == t))
                    .collect();
                if tools.is_empty() {
                    self.editor().message = Some(
                        "Every tool this stage has is routed already; give it a tool first"
                            .to_string(),
                    );
                    return;
                }
                let rows = tools
                    .into_iter()
                    .map(|value| PickerOption {
                        value,
                        detail: String::new(),
                    })
                    .collect();
                let picker = Picker::new(
                    "Route which tool?",
                    vec!["Its results will land in a region of their own.".to_string()],
                    rows,
                    0,
                );
                self.editor().picker = Some((PickerFor::RoutingTool, picker));
            }
            FieldId::AddArtifact => {
                self.editor().add_artifact = Some(LineEdit::new(String::new(), false));
            }
            FieldId::DeleteArtifact => {
                if let Some((stage, i)) = self.panel_artifact()
                    && self.editor_mutate(|d| d.delete_artifact(&stage, i))
                {
                    self.editor_close_modal();
                }
            }
            FieldId::DeleteRegion => {
                let Some((scope, name)) = self.panel_region() else {
                    return;
                };
                let routed = self.editor().doc.stages_routing_into(&name);
                let mut lines = vec![Line::from(format!("Delete the region '{name}'?"))];
                if !routed.is_empty() {
                    lines.push(Line::from(format!(
                        "Tool results of {} land in it; that routing goes too.",
                        routed.join(", ")
                    )));
                }
                let dialog = Confirm::new("Delete region?", lines, "Delete", "Cancel").danger();
                self.pending_confirm = Some((ConfirmAction::RegionDelete { scope, name }, dialog));
            }
            _ => {}
        }
    }

    /// The confirmed region delete.
    pub(in crate::commands::dashboard) fn editor_delete_region(
        &mut self,
        scope: &RegionScope,
        name: &str,
    ) {
        let (scope, name) = (scope.clone(), name.to_string());
        // The window closes with the region; the refresh would have closed
        // it anyway, and this way the cursor lands where it was.
        if self.editor_mutate(|d| d.delete_region(&scope, &name)) {
            self.editor_close_modal();
        }
    }

    /// The confirmed removal of a stage's own layout.
    pub(in crate::commands::dashboard) fn editor_remove_override(&mut self, stage: &str) {
        let stage = stage.to_string();
        self.editor_mutate(|d| d.remove_stage_override(&stage));
    }

    /// A typed line the core does not know.
    pub(super) fn editor_commit_line_more(&mut self, id: &FieldId, text: &str) {
        let text = text.to_string();
        match id {
            FieldId::CompactPrompt => {
                let (from, to) = self.editor().panel_edge().expect("a path field");
                self.editor_mutate(|d| d.set_compact_prompt(&from, &to, &text));
            }
            FieldId::RegionName => {
                let Some((scope, name)) = self.panel_region() else {
                    return;
                };
                // The panel follows the new name before the document changes,
                // so the refresh finds the region it shows; a refusal puts
                // the old name back.
                self.set_region_panel_name(&text);
                if !self.editor_mutate(|d| d.rename_region(&scope, &name, &text)) {
                    self.set_region_panel_name(&name);
                }
            }
            // A typed list of types: the chooser's "another…" row lands
            // here with what was picked already in the box.
            FieldId::StageAccepts
            | FieldId::StageAsText
            | FieldId::RegionAccepts
            | FieldId::ToolLimitRow(_)
            | FieldId::ArtifactType => self.editor_write_types(id, split_list(&text)),
            FieldId::OutputFormat => {
                let stage = self.editor().panel_stage().expect("a stage field");
                self.editor_mutate(|d| d.set_output_format(&stage, &text));
            }
            FieldId::ArtifactName | FieldId::ArtifactDescription => {
                let Some((stage, i)) = self.panel_artifact() else {
                    return;
                };
                let field = match id {
                    FieldId::ArtifactName => ArtifactField::Name(text),
                    _ => ArtifactField::Description(text),
                };
                self.editor_mutate(|d| d.set_artifact(&stage, i, field));
            }
            FieldId::RegionStrategy
            | FieldId::RegionMessage
            | FieldId::RegionSeed
            | FieldId::RegionDescription => {
                let field = match id {
                    FieldId::RegionStrategy => RegionField::Strategy,
                    FieldId::RegionMessage => RegionField::RequiredMessage,
                    FieldId::RegionSeed => RegionField::Seed,
                    _ => RegionField::Description,
                };
                if let Some((scope, name)) = self.panel_region() {
                    self.editor_mutate(|d| {
                        d.set_region_field(&scope, &name, field, RegionValue::Text(text))
                    });
                }
            }
            _ => {}
        }
    }

    /// The name the region panel shows.
    pub(super) fn set_region_panel_name(&mut self, name: &str) {
        if let Panel::Region { name: shown, .. } = &mut self.editor().panel {
            *shown = name.to_string();
        }
    }

    /// Enter on a row: a region opens its panel, a model entry swaps it, the
    /// tools open the multi-chooser, a routing row changes its region.
    pub(super) fn editor_open_row(&mut self, id: &FieldId) {
        match id {
            FieldId::RegionRow(name) => {
                self.editor_open_region(RegionScope::Shared, name);
            }
            FieldId::StageRegionRow(name) | FieldId::IoRegionRow(name) => {
                let stage = self.editor().panel_stage().expect("a stage field");
                let scope = if self.editor().doc.effective_regions(Some(&stage)).inherited {
                    RegionScope::Shared
                } else {
                    RegionScope::Stage(stage)
                };
                self.editor_open_region(scope, name);
            }
            FieldId::ArtifactRow(i) => {
                let stage = self.editor().panel_stage().expect("a stage field");
                self.editor_open_artifact(&stage, *i);
            }
            FieldId::StageAccepts
            | FieldId::StageAsText
            | FieldId::RegionAccepts
            | FieldId::ToolLimitRow(_)
            | FieldId::ArtifactType => self.editor_open_type_chooser(id),
            FieldId::ModelEntry(i) => self.editor_open_model_picker(PickerFor::ReplaceModel(*i)),
            // The empty chain reads as a model row; Enter still adds.
            FieldId::AddModel => self.editor_open_model_picker(PickerFor::AddModel),
            FieldId::ToolSet => self.editor_open_tools_picker(),
            FieldId::RoutingRow(tool) => self.editor_open_routing_region_picker(tool),
            FieldId::SelfLoop => {
                let stage = self.editor().panel_stage().expect("a stage field");
                self.editor_open_self_loop(&stage);
            }
            _ => {}
        }
    }

    /// `x` on a row: drop a model from the chain, stop routing a tool, drop
    /// an artifact declaration.
    pub(super) fn editor_remove_row(&mut self) {
        let Some(field) = self.editor().current_field() else {
            return;
        };
        match field.id {
            FieldId::ArtifactRow(i) => {
                let stage = self.editor().panel_stage().expect("a stage field");
                self.editor_mutate(|d| d.delete_artifact(&stage, i));
            }
            // A list of types cleared: the stage's, a tool's, a region's.
            FieldId::StageAccepts
            | FieldId::StageAsText
            | FieldId::RegionAccepts
            | FieldId::ToolLimitRow(_) => self.editor_write_types(&field.id, Vec::new()),
            FieldId::ModelEntry(i) => {
                let stage = self.editor().panel_stage().expect("a stage field");
                let chain: Vec<String> = self
                    .editor()
                    .doc
                    .stage(&stage)
                    .map(|s| s.models)
                    .unwrap_or_default()
                    .into_iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .map(|(_, m)| m)
                    .collect();
                self.editor_mutate(|d| d.set_models(&stage, &chain));
            }
            FieldId::RoutingRow(tool) => {
                let stage = self.editor().panel_stage().expect("a stage field");
                self.editor_mutate(|d| d.set_tool_routing_override(&stage, &tool, ""));
            }
            _ => {}
        }
    }

    /// `←`/`→` on a model entry: move it one place along the chain.
    pub(super) fn editor_move_model(&mut self, index: usize, delta: isize) {
        let Some(to) = index.checked_add_signed(delta) else {
            return;
        };
        self.editor_reorder_model(index, to);
    }

    /// Put the chain's `from`th entry at `to`, the cursor following it.
    ///
    /// Lift-and-insert rather than a swap. The two agree for the one-step
    /// move the arrow keys make, but a drag crosses several rows at once, and
    /// swapping its ends would fling the entry the pointer had just passed
    /// all the way back to where the dragged one started.
    ///
    /// Out-of-range and standing-still are both no-ops, so a drop back where
    /// it began costs nothing - not even an undo entry.
    ///
    /// The cursor is a field index and `from`/`to` are chain indices; on the
    /// model tab the chain is drawn first, so the two are the same number and
    /// the shift applies directly.
    pub(super) fn editor_reorder_model(&mut self, from: usize, to: usize) {
        let stage = self.editor().panel_stage().expect("a stage field");
        let mut chain = self
            .editor()
            .doc
            .stage(&stage)
            .map(|s| s.models)
            .unwrap_or_default();
        if from >= chain.len() || to >= chain.len() || from == to {
            return;
        }
        let entry = chain.remove(from);
        chain.insert(to, entry);
        // The stage is the one the panel shows, so the write cannot be
        // refused.
        self.editor_mutate(|d| d.set_models(&stage, &chain));
        let editor = self.editor();
        editor.cursor = (editor.cursor + to).saturating_sub(from);
    }

    /// `←`/`→`/Enter on a segment: the next rule for the region.
    pub(super) fn editor_cycle_segment(&mut self, id: &FieldId, delta: isize) {
        let FieldId::TransformRule(region) = id else {
            return;
        };
        let (from, to) = self.editor().panel_edge().expect("a path field");
        let Some(edge) = self.editor().doc.edge(&from, &to) else {
            return;
        };
        let current = Rule::ALL.iter().position(|r| match r {
            Rule::Carry => edge.rules.carry.contains(region),
            Rule::Compact => edge.rules.compact.contains(region),
            Rule::Clear => edge.rules.clear.contains(region),
        });
        let next = match current {
            Some(i) => (i as isize + delta).rem_euclid(Rule::ALL.len() as isize) as usize,
            None if delta < 0 => Rule::ALL.len() - 1,
            None => 0,
        };
        let rule = Rule::ALL[next];
        let region = region.clone();
        self.editor_mutate(|d| d.set_transform_rule(&from, &to, &region, rule));
    }

    /// Open `panel` in a window over the panel the inspector shows, which
    /// stays where it is until the window closes. A window never opens over
    /// another: every row that opens one sits on a panel the inspector
    /// shows, so the panel here is always the one to come back to.
    fn editor_open_modal(&mut self, panel: Panel) {
        let editor = self.editor();
        editor.modal = Some(ModalBase {
            panel: editor.panel.clone(),
            cursor: editor.cursor,
            anchor: editor.view.selection(),
        });
        editor.panel = panel;
        editor.cursor = 0;
        editor.focus = super::editor::Focus::Inspector;
    }

    /// Open a region's window.
    pub(super) fn editor_open_region(&mut self, scope: RegionScope, name: &str) {
        self.editor_open_modal(Panel::Region {
            scope,
            name: name.to_string(),
        });
    }

    /// Open a declared file's window.
    pub(super) fn editor_open_artifact(&mut self, stage: &str, index: usize) {
        self.editor_open_modal(Panel::Artifact {
            stage: stage.to_string(),
            index,
        });
    }

    /// Open the path panel on a stage's loop back to itself. A loop is not
    /// a line on the canvas (the box wears a badge instead), so this is how
    /// it is reached: from the stage's behaviour tab, or right after `c`
    /// connects a stage to itself.
    pub(super) fn editor_open_self_loop(&mut self, stage: &str) {
        let editor = self.editor();
        editor.view.select_stage(stage);
        // The window opens over the stage's behaviour tab, whichever panel
        // led here: the canvas menu, or a fresh loop just connected.
        editor.modal = None;
        editor.panel = Panel::Stage {
            name: stage.to_string(),
            tab: super::inspector::StageTab::Behaviour,
        };
        editor.cursor = 0;
        self.editor_open_modal(Panel::Edge {
            from: stage.to_string(),
            to: stage.to_string(),
        });
    }

    /// Esc on a window: back to the panel it was opened over. Nothing
    /// happens when no window is up.
    pub(super) fn editor_close_modal(&mut self) {
        let editor = self.editor();
        let Some(base) = editor.modal.take() else {
            return;
        };
        editor.panel = base.panel;
        editor.cursor = base.cursor;
        let count = editor.fields().len();
        editor.cursor = editor.cursor.min(count.saturating_sub(1));
    }

    /// What a type field holds now, whether it takes one type or many, and
    /// what the chooser is called.
    fn type_field_state(&mut self, id: &FieldId) -> (Vec<String>, bool, String, String) {
        let stage = self.editor().panel_stage();
        let view = stage.as_deref().and_then(|s| self.editor().doc.stage(s));
        match id {
            FieldId::StageAccepts => (
                view.map(|s| s.input_accepts).unwrap_or_default(),
                false,
                "What the stage takes, beyond its regions".to_string(),
                "Media type patterns the stage takes as parts. Left empty, its regions decide."
                    .to_string(),
            ),
            FieldId::StageAsText => (
                view.map(|s| s.input_as_text).unwrap_or_default(),
                false,
                "What the stage reads as text".to_string(),
                "Types whose parts reach the model as text whatever it takes natively.".to_string(),
            ),
            FieldId::ToolLimitRow(tool) => (
                view.and_then(|s| {
                    s.tool_accepts
                        .into_iter()
                        .find(|(t, _)| t == tool)
                        .map(|(_, list)| list)
                })
                .unwrap_or_default(),
                false,
                format!("What {tool} may be handed here"),
                "A part of any other type is out of the tool's reach at this stage; nothing \
                 picked means whatever the tool takes."
                    .to_string(),
            ),
            FieldId::ArtifactType => (
                self.panel_artifact()
                    .and_then(|(s, i)| self.editor().doc.artifacts(&s).into_iter().nth(i))
                    .map(|a| vec![a.media_type])
                    .unwrap_or_default(),
                true,
                "The file's type".to_string(),
                "The media type the file must be, or a pattern it must match.".to_string(),
            ),
            _ => (
                self.panel_region()
                    .and_then(|(scope, name)| self.editor().doc.region(scope.stage(), &name))
                    .map(|r| r.accepts)
                    .unwrap_or_default(),
                false,
                "What the region takes".to_string(),
                "Media type patterns the region takes; a write outside them is refused with \
                 the list. Nothing picked takes anything."
                    .to_string(),
            ),
        }
    }

    /// The media type chooser for a field: every family, every type the
    /// registry knows, whatever the field already holds, and a row to type
    /// one in.
    fn editor_open_type_chooser(&mut self, id: &FieldId) {
        let (current, single, title, explain) = self.type_field_state(id);
        let mut values = self.editor().media_types.clone();
        for held in &current {
            if !values.contains(held) {
                values.push(held.clone());
            }
        }
        let mut rows: Vec<PickerOption> = values
            .iter()
            .map(|value| PickerOption {
                value: value.clone(),
                detail: type_detail(value),
            })
            .collect();
        rows.push(PickerOption {
            value: TYPE_ANOTHER.to_string(),
            detail: "a type/subtype or type/* the list does not have".to_string(),
        });
        let cursor = current
            .first()
            .and_then(|c| values.iter().position(|v| v == c))
            .unwrap_or(0);
        let mut picker = Picker::new(title, vec![explain], rows, cursor);
        if !single {
            picker.multi = Some(
                values
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| current.contains(v))
                    .map(|(i, _)| i)
                    .collect(),
            );
        }
        self.editor().picker = Some((PickerFor::MediaTypes(id.clone()), picker));
    }

    /// What the type chooser settled on. The "another…" row opens the line
    /// editor with the rest already in it, so one more can be typed.
    pub(super) fn editor_settle_types(&mut self, id: &FieldId, chosen: Vec<String>) {
        let (kept, another): (Vec<String>, Vec<String>) =
            chosen.into_iter().partition(|v| v != TYPE_ANOTHER);
        if !another.is_empty() {
            let mut text = kept.join(", ");
            if !text.is_empty() {
                text.push_str(", ");
            }
            self.editor().line = Some((id.clone(), LineEdit::new(text, false)));
            return;
        }
        self.editor_write_types(id, kept);
    }

    /// Write a list of types to the field it belongs to.
    fn editor_write_types(&mut self, id: &FieldId, types: Vec<String>) {
        match id {
            FieldId::StageAccepts | FieldId::StageAsText => {
                let stage = self.editor().panel_stage().expect("a stage field");
                let which = if *id == FieldId::StageAccepts {
                    InputList::Accepts
                } else {
                    InputList::AsText
                };
                self.editor_mutate(|d| d.set_stage_input(&stage, which, &types));
            }
            FieldId::ToolLimitRow(tool) => {
                let stage = self.editor().panel_stage().expect("a stage field");
                self.editor_mutate(|d| d.set_tool_accepts(&stage, tool, &types));
            }
            FieldId::ArtifactType => {
                let Some((stage, i)) = self.panel_artifact() else {
                    return;
                };
                let Some(first) = types.into_iter().next() else {
                    return;
                };
                self.editor_mutate(|d| d.set_artifact(&stage, i, ArtifactField::Type(first)));
            }
            _ => {
                if let Some((scope, name)) = self.panel_region() {
                    self.editor_mutate(|d| {
                        d.set_region_field(
                            &scope,
                            &name,
                            RegionField::Accepts,
                            RegionValue::Text(types.join(", ")),
                        )
                    });
                }
            }
        }
    }

    /// The model chooser: every model this install knows, live ones first.
    fn editor_open_model_picker(&mut self, purpose: PickerFor) {
        let windows = crate::commands::models::builtin_model_windows();
        let live: BTreeSet<String> = self.agents().model_catalog.iter().cloned().collect();
        let named: BTreeSet<String> = self.editor().doc.known_models().into_iter().collect();
        let rows: Vec<PickerOption> = self
            .editor()
            .models
            .iter()
            .map(|m| {
                let (provider, id) = m.split_once('/').unwrap_or(("", m.as_str()));
                let mut detail: Vec<String> = Vec::new();
                if live.contains(m) {
                    detail.push("your provider lists it".to_string());
                }
                if named.contains(m) {
                    detail.push("already in this agent".to_string());
                }
                if let Some(window) = windows.get(&(provider.to_string(), id.to_string())) {
                    detail.push(format!("{} context", window_label(*window)));
                }
                PickerOption {
                    value: m.clone(),
                    detail: detail.join(" · "),
                }
            })
            .collect();
        let picker = Picker::new(
            "Which model?",
            vec![
                "Written as provider/model. A run uses the first one in the chain that a configured \
                 provider can serve."
                    .to_string(),
            ],
            rows,
            0,
        );
        self.editor().picker = Some((purpose, picker));
    }

    /// The tools multi-chooser, preselected with the stage's tools.
    fn editor_open_tools_picker(&mut self) {
        let stage = self.editor().panel_stage().expect("a stage field");
        let have = self
            .editor()
            .doc
            .stage(&stage)
            .map(|s| s.tools)
            .unwrap_or_default();
        let all = self.editor().tools.clone();
        let rows: Vec<PickerOption> = all
            .iter()
            .map(|t| PickerOption {
                value: t.name.clone(),
                detail: t.detail.clone(),
            })
            .collect();
        let chosen: Vec<usize> = all
            .iter()
            .enumerate()
            .filter(|(_, t)| have.contains(&t.name))
            .map(|(i, _)| i)
            .collect();
        let mut picker = Picker::new(
            format!("Tools {stage} may use"),
            vec![
                "A group such as @builtin grants every tool of that kind, installed now or \
                 later; the rest are the tools this install has, one by one."
                    .to_string(),
            ],
            rows,
            0,
        );
        picker.multi = Some(chosen.into_iter().collect());
        self.editor().picker = Some((PickerFor::Tools, picker));
    }

    /// The region chooser for a routing row, or for a tool just picked.
    fn editor_open_routing_region_picker(&mut self, tool: &str) {
        let stage = self.editor().panel_stage().expect("a stage field");
        let options = self.routing_regions(&stage);
        let rows: Vec<PickerOption> = options
            .into_iter()
            .skip(1)
            .map(|value| PickerOption {
                value,
                detail: String::new(),
            })
            .collect();
        let picker = Picker::new(
            format!("{tool}'s results land in"),
            vec!["A region the stage sees.".to_string()],
            rows,
            0,
        );
        self.editor().picker = Some((PickerFor::RoutingRegion(tool.to_string()), picker));
    }

    /// A pick from the choosers the core does not settle itself.
    pub(super) fn editor_settle_more(&mut self, purpose: PickerFor, value: &str) {
        match purpose {
            PickerFor::AddModel | PickerFor::ReplaceModel(_) => {
                let stage = self.editor().panel_stage().expect("a stage field");
                let mut chain = self
                    .editor()
                    .doc
                    .stage(&stage)
                    .map(|s| s.models)
                    .unwrap_or_default();
                match purpose {
                    PickerFor::ReplaceModel(i) if i < chain.len() => chain[i] = value.to_string(),
                    _ => chain.push(value.to_string()),
                }
                self.editor_mutate(|d| d.set_models(&stage, &chain));
            }
            PickerFor::RoutingTool => self.editor_open_routing_region_picker(value),
            PickerFor::RoutingRegion(tool) => {
                let stage = self.editor().panel_stage().expect("a stage field");
                let region = value.to_string();
                self.editor_mutate(|d| d.set_tool_routing_override(&stage, &tool, &region));
            }
            PickerFor::Tools
            | PickerFor::Field(_)
            | PickerFor::ConnectFrom(_)
            | PickerFor::MediaTypes(_) => {}
        }
    }

    /// The tools chosen in the multi-chooser.
    pub(super) fn editor_settle_tools(&mut self, chosen: &[usize]) {
        let stage = self.editor().panel_stage().expect("a stage field");
        let all = self.editor().tools.clone();
        let tools: Vec<String> = chosen
            .iter()
            .filter_map(|i| all.get(*i).map(|t| t.name.clone()))
            .collect();
        self.editor_mutate(|d| d.set_tools(&stage, &tools));
    }

    /// Enter on the add-artifact prompt: a new declaration on the stage the
    /// panel shows, opened in its window so its type can be picked at once.
    pub(super) fn editor_add_artifact(&mut self, name: &str) {
        let stage = self.editor().panel_stage().expect("a stage field");
        let name = name.to_string();
        if self.editor_mutate(|d| d.add_artifact(&stage, &name)) {
            let last = self.editor().doc.artifacts(&stage).len().saturating_sub(1);
            self.editor_open_artifact(&stage, last);
        }
    }

    /// Enter on the add-region prompt: a new region in the stage's own
    /// layout (or the agent's, from the agent panel).
    pub(super) fn editor_add_region(&mut self, name: &str) {
        let scope = match self.editor().panel_stage() {
            Some(stage) => RegionScope::Stage(stage),
            None => RegionScope::Shared,
        };
        let name = name.to_string();
        if self.editor_mutate(|d| d.add_region(&scope, &name)) {
            self.editor_open_region(scope, &name);
        }
    }
}
