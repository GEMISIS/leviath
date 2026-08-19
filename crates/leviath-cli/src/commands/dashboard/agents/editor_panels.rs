//! The inspector's other panels: a stage's model chain and tools, its
//! context layout and tool routing, a region, and a path's transform. The
//! core (`editor.rs`) hands a field it does not know to the `*_more`
//! functions here.

use std::collections::BTreeSet;

use super::super::state::Dashboard;
use super::super::types::ConfirmAction;
use super::editor::PickerFor;
use super::inspector::{FieldId, Panel, REGION_KINDS};
use crate::blueprint_edit::{RegionField, RegionScope, RegionValue, Rule, TransformKind};
use crate::tui::widgets::confirm::Confirm;
use crate::tui::widgets::line_edit::LineEdit;
use crate::tui::widgets::picker::{Picker, PickerOption};
use ratatui::text::Line;

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
            Panel::Region { scope, name, .. } => Some((scope.clone(), name.clone())),
            _ => None,
        }
    }

    /// A toggle the core does not know: the region's `required`.
    pub(super) fn editor_set_toggle_more(&mut self, id: &FieldId, on: bool) {
        if *id == FieldId::RegionRequired
            && let Some((scope, name)) = self.panel_region()
        {
            self.editor_mutate(|d| {
                d.set_region_field(&scope, &name, RegionField::Required, RegionValue::Flag(on))
            });
        }
    }

    /// A number the core does not know: the region's sizes.
    pub(super) fn editor_set_number_more(&mut self, id: &FieldId, value: Option<u64>) -> bool {
        let field = match id {
            FieldId::RegionBudget => RegionField::BudgetPercent,
            FieldId::RegionMaxTokens => RegionField::MaxTokens,
            FieldId::RegionMaxItems => RegionField::MaxItems,
            FieldId::RegionOverflow => RegionField::Overflow,
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
        // Where the panel came from, taken before the refresh forgets it.
        let back = match &self.editor().panel {
            Panel::Region { back, .. } => Some((**back).clone()),
            _ => None,
        };
        if self.editor_mutate(|d| d.delete_region(&scope, &name))
            && let Some(back) = back
        {
            let editor = self.editor();
            editor.panel = back;
            editor.panel_anchor = None;
            editor.cursor = 0;
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
            FieldId::StageRegionRow(name) => {
                let stage = self.editor().panel_stage().expect("a stage field");
                let scope = if self.editor().doc.effective_regions(Some(&stage)).inherited {
                    RegionScope::Shared
                } else {
                    RegionScope::Stage(stage)
                };
                self.editor_open_region(scope, name);
            }
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

    /// `x` on a row: drop a model from the chain, stop routing a tool.
    pub(super) fn editor_remove_row(&mut self) {
        let Some(field) = self.editor().current_field() else {
            return;
        };
        match field.id {
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

    /// `←`/`→` on a model entry: move it in the chain.
    pub(super) fn editor_move_model(&mut self, index: usize, delta: isize) {
        let stage = self.editor().panel_stage().expect("a stage field");
        let mut chain = self
            .editor()
            .doc
            .stage(&stage)
            .map(|s| s.models)
            .unwrap_or_default();
        let to = index as isize + delta;
        if index >= chain.len() || to < 0 || to as usize >= chain.len() {
            return;
        }
        chain.swap(index, to as usize);
        // The stage is the one the panel shows, so the write cannot be
        // refused; the cursor follows the entry.
        self.editor_mutate(|d| d.set_models(&stage, &chain));
        let editor = self.editor();
        editor.cursor = (editor.cursor as isize + delta) as usize;
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

    /// Open the region panel, remembering where to go back to.
    pub(super) fn editor_open_region(&mut self, scope: RegionScope, name: &str) {
        let editor = self.editor();
        let back = Box::new(editor.panel.clone());
        editor.panel_anchor = Some(editor.view.selection());
        editor.panel = Panel::Region {
            scope,
            name: name.to_string(),
            back,
        };
        editor.cursor = 0;
    }

    /// Open the path panel on a stage's loop back to itself. A loop is not
    /// a line on the canvas (the box wears a badge instead), so this is how
    /// it is reached: from the stage's behaviour tab, or right after `c`
    /// connects a stage to itself.
    pub(super) fn editor_open_self_loop(&mut self, stage: &str) {
        let editor = self.editor();
        editor.view.select_stage(stage);
        editor.panel_anchor = Some(editor.view.selection());
        editor.panel = Panel::Edge {
            from: stage.to_string(),
            to: stage.to_string(),
        };
        editor.cursor = 0;
        editor.focus = super::editor::Focus::Inspector;
    }

    /// Esc on a pushed panel: back to what opened it.
    pub(super) fn editor_leave_region(&mut self) {
        let editor = self.editor();
        if let Panel::Edge { from, .. } = &editor.panel {
            let name = from.clone();
            editor.panel = Panel::Stage {
                name,
                tab: super::inspector::StageTab::Behaviour,
            };
            editor.panel_anchor = None;
            editor.cursor = 0;
            return;
        }
        if let Panel::Region { back, .. } = &editor.panel {
            editor.panel = (**back).clone();
            editor.panel_anchor = None;
            editor.cursor = 0;
            let count = editor.fields().len();
            editor.cursor = editor.cursor.min(count.saturating_sub(1));
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
                value: t.clone(),
                detail: String::new(),
            })
            .collect();
        let chosen: Vec<usize> = all
            .iter()
            .enumerate()
            .filter(|(_, t)| have.contains(t))
            .map(|(i, _)| i)
            .collect();
        let mut picker = Picker::new(
            format!("Tools {stage} may use"),
            vec!["Every tool this install has: built in, and scripts under the agent.".to_string()],
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
            PickerFor::Tools | PickerFor::Field(_) | PickerFor::ConnectFrom(_) => {}
        }
    }

    /// The tools chosen in the multi-chooser.
    pub(super) fn editor_settle_tools(&mut self, chosen: &[usize]) {
        let stage = self.editor().panel_stage().expect("a stage field");
        let all = self.editor().tools.clone();
        let tools: Vec<String> = chosen.iter().filter_map(|i| all.get(*i).cloned()).collect();
        self.editor_mutate(|d| d.set_tools(&stage, &tools));
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
