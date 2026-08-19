//! Mutators for `[agent]` and the `[stages.<name>]` tables.

use toml_edit::{Array, InlineTable, Item, Value};

use super::doc::{ManifestDoc, StageModeView, WorkerKind};
use super::tables::{
    as_table, child_mut, ensure_child, get_str, new_table, rename_key, set_bool, set_int,
    set_or_remove_str, set_str, set_strings,
};
use super::{EditError, order, require_name};

/// The free-text keys of a stage the editor writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageText {
    /// `description`.
    Description,
    /// `system_prompt`.
    SystemPrompt,
    /// `transition_prompt`.
    TransitionPrompt,
}

impl StageText {
    fn key(self) -> &'static str {
        match self {
            StageText::Description => "description",
            StageText::SystemPrompt => "system_prompt",
            StageText::TransitionPrompt => "transition_prompt",
        }
    }
}

/// One fan-out key of a stage. `None` deletes the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FanOutField {
    /// Which of `worker_agent`/`worker_stage`/`worker_query` is written, and
    /// its value; the other two are removed, because the runtime wants
    /// exactly one.
    Worker(Option<(WorkerKind, String)>),
    /// `merge_stage`.
    MergeStage(Option<String>),
    /// `max_workers`.
    MaxWorkers(Option<u64>),
    /// `max_items`.
    MaxItems(Option<u64>),
    /// `on_worker_failure`.
    OnWorkerFailure(Option<String>),
}

const FAN_OUT_KEYS: [&str; 9] = [
    "worker_agent",
    "worker_stage",
    "worker_query",
    "merge_stage",
    "max_workers",
    "max_items",
    "on_worker_failure",
    "split_prompt",
    "results_region",
];

impl ManifestDoc {
    /// Set `[agent].name`.
    pub fn set_agent_name(&mut self, name: &str) -> Result<(), EditError> {
        require_name(name)?;
        let agent = self
            .doc_mut()
            .get_mut("agent")
            .and_then(Item::as_table_like_mut)
            .expect("parse() checked [agent] is a table");
        set_str(agent, "name", name);
        Ok(())
    }

    /// Set `[agent].description`; empty deletes it.
    pub fn set_description(&mut self, text: &str) {
        let agent = self
            .doc_mut()
            .get_mut("agent")
            .and_then(Item::as_table_like_mut)
            .expect("parse() checked [agent] is a table");
        set_or_remove_str(agent, "description", text);
    }

    /// Point `[agent].entry_stage` at `stage`, which must exist.
    pub fn set_entry_stage(&mut self, stage: &str) -> Result<(), EditError> {
        self.require_stage(stage)?;
        let agent = self
            .doc_mut()
            .get_mut("agent")
            .and_then(Item::as_table_like_mut)
            .expect("parse() checked [agent] is a table");
        set_str(agent, "entry_stage", stage);
        Ok(())
    }

    /// Make `model` (`provider/model`) the first model every stage tries,
    /// keeping the rest of each stage's chain behind it.
    pub fn set_default_model(&mut self, model: &str) {
        for stage in self.stages() {
            let mut chain: Vec<String> = vec![model.to_string()];
            chain.extend(stage.models.into_iter().filter(|m| m != model));
            self.set_models(&stage.name, &chain)
                .expect("a listed stage exists");
        }
    }

    /// Add a stage after `after` (or at the end): autonomous, twenty tries,
    /// nowhere to go yet.
    pub fn add_stage(&mut self, name: &str, after: Option<&str>) -> Result<(), EditError> {
        require_name(name)?;
        if self.stages_key_taken(name) {
            return Err(EditError::Taken(name.to_string()));
        }
        // Without an anchor the new stage goes after the last one; a table
        // with no position of its own would otherwise be written wherever the
        // writer last was, which after a reorder is not the end.
        let after = match after {
            Some(a) => {
                self.require_stage(a)?;
                a.to_string()
            }
            None => self
                .stage_names()
                .last()
                .cloned()
                .expect("parse() checked a stage exists"),
        };
        let stages = self
            .doc_mut()
            .get_mut("stages")
            .expect("parse() checked a stage exists");
        let inline = stages.is_inline_table();
        let mut stage = new_table(inline);
        {
            let table = stage.as_table_like_mut().expect("just built a table");
            set_str(table, "mode", "autonomous");
            set_int(table, "max_iterations", 20);
            table.insert("transitions", new_table(inline));
        }
        stages
            .as_table_like_mut()
            .expect("parse() checked [stages] is a table")
            .insert(name, stage);
        order::move_stage_block(self.doc_mut(), name, &after, false);
        Ok(())
    }

    /// Rename a stage, rewriting every path into it, `entry_stage`, and any
    /// `worker_stage`/`merge_stage` naming it.
    pub fn rename_stage(&mut self, from: &str, to: &str) -> Result<(), EditError> {
        if from == to {
            return Ok(());
        }
        require_name(to)?;
        self.require_stage(from)?;
        if self.stages_key_taken(to) {
            return Err(EditError::Taken(to.to_string()));
        }
        let names = self.stage_names();
        let stages = self
            .doc_mut()
            .get_mut("stages")
            .and_then(Item::as_table_like_mut)
            .expect("parse() checked [stages] is a table");
        rename_key(stages, from, to);
        for name in &names {
            let stage = self
                .stage_item_mut(if name == from { to } else { name })
                .expect("every listed stage exists");
            if let Some(transitions) = child_mut(stage, "transitions") {
                rename_key(
                    transitions.as_table_like_mut().expect("child_mut checked"),
                    from,
                    to,
                );
            }
            let table = stage.as_table_like_mut().expect("a stage is a table");
            for key in ["worker_stage", "merge_stage"] {
                if get_str(table, key) == Some(from) {
                    set_str(table, key, to);
                }
            }
        }
        let agent = self
            .doc_mut()
            .get_mut("agent")
            .and_then(Item::as_table_like_mut)
            .expect("parse() checked [agent] is a table");
        if get_str(agent, "entry_stage") == Some(from) {
            set_str(agent, "entry_stage", to);
        }
        Ok(())
    }

    /// Delete a stage and every path into it. Refuses the last stage; deleting
    /// the entry stage points the entry at the first stage left.
    pub fn delete_stage(&mut self, name: &str) -> Result<(), EditError> {
        self.require_stage(name)?;
        let remaining: Vec<String> = self
            .stage_names()
            .into_iter()
            .filter(|n| n != name)
            .collect();
        let Some(first) = remaining.first().cloned() else {
            return Err(EditError::LastStage);
        };
        self.doc_mut()
            .get_mut("stages")
            .and_then(Item::as_table_like_mut)
            .expect("parse() checked [stages] is a table")
            .remove(name);
        for other in &remaining {
            let stage = self.stage_item_mut(other).expect("listed stage exists");
            if let Some(transitions) = child_mut(stage, "transitions") {
                transitions
                    .as_table_like_mut()
                    .expect("child_mut checked")
                    .remove(name);
            }
        }
        let agent = self
            .doc_mut()
            .get_mut("agent")
            .and_then(Item::as_table_like_mut)
            .expect("parse() checked [agent] is a table");
        if get_str(agent, "entry_stage") == Some(name) {
            set_str(agent, "entry_stage", &first);
        }
        Ok(())
    }

    /// Swap a stage with its neighbour in the file (`up` = towards the
    /// start). At either end nothing happens.
    pub fn move_stage(&mut self, name: &str, up: bool) -> Result<(), EditError> {
        self.require_stage(name)?;
        let names = self.stage_names();
        let index = names.iter().position(|n| n == name).expect("listed");
        let other = if up {
            index.checked_sub(1).map(|i| &names[i])
        } else {
            names.get(index + 1)
        };
        if let Some(other) = other {
            order::move_stage_block(self.doc_mut(), name, other, up);
        }
        Ok(())
    }

    /// Set a stage's `mode`. Leaving `fan_out` deletes the fan-out keys.
    pub fn set_stage_mode(&mut self, name: &str, mode: &StageModeView) -> Result<(), EditError> {
        let stage = self.stage_table_mut(name)?;
        set_str(stage, "mode", mode.as_str());
        if *mode != StageModeView::FanOut {
            for key in FAN_OUT_KEYS {
                stage.remove(key);
            }
        }
        Ok(())
    }

    /// Set one of a stage's text keys; empty deletes it.
    pub fn set_stage_text(
        &mut self,
        name: &str,
        which: StageText,
        text: &str,
    ) -> Result<(), EditError> {
        let stage = self.stage_table_mut(name)?;
        set_or_remove_str(stage, which.key(), text);
        Ok(())
    }

    /// Set `max_iterations` (at least 1); `None` deletes it.
    pub fn set_max_iterations(&mut self, name: &str, value: Option<u64>) -> Result<(), EditError> {
        let stage = self.stage_table_mut(name)?;
        match value {
            Some(n) => set_int(stage, "max_iterations", clamp_i64(n.max(1))),
            None => {
                stage.remove("max_iterations");
            }
        }
        Ok(())
    }

    /// Set `max_revisits`; `None` deletes it.
    pub fn set_max_revisits(&mut self, name: &str, value: Option<u64>) -> Result<(), EditError> {
        let stage = self.stage_table_mut(name)?;
        match value {
            Some(n) => set_int(stage, "max_revisits", clamp_i64(n)),
            None => {
                stage.remove("max_revisits");
            }
        }
        Ok(())
    }

    /// Set `allow_complete`; `None` deletes it (the runtime's default).
    pub fn set_allow_complete(&mut self, name: &str, value: Option<bool>) -> Result<(), EditError> {
        let stage = self.stage_table_mut(name)?;
        match value {
            Some(b) => set_bool(stage, "allow_complete", b),
            None => {
                stage.remove("allow_complete");
            }
        }
        Ok(())
    }

    /// Set a stage's model chain (`provider/model` each) as
    /// `model = { models = [...] }`, keeping any other key of an existing
    /// `model` table. An empty chain deletes `model`.
    pub fn set_models(&mut self, name: &str, chain: &[String]) -> Result<(), EditError> {
        let stage = self.stage_table_mut(name)?;
        if chain.is_empty() {
            stage.remove("model");
            return Ok(());
        }
        let mut models = Array::new();
        for entry in chain {
            let (provider, model) = entry.split_once('/').unwrap_or(("", entry.as_str()));
            let mut t = InlineTable::new();
            t.insert("provider", Value::from(provider));
            t.insert("model", Value::from(model));
            models.push(Value::InlineTable(t));
        }
        let keeps_table = stage
            .get("model")
            .is_some_and(|m| m.as_table_like().is_some());
        if !keeps_table {
            stage.insert("model", Item::Value(Value::InlineTable(InlineTable::new())));
        }
        let model = stage
            .get_mut("model")
            .and_then(Item::as_table_like_mut)
            .expect("a table now");
        model.insert("models", Item::Value(Value::Array(models)));
        Ok(())
    }

    /// Set `available_tools`; an empty list deletes it.
    pub fn set_tools(&mut self, name: &str, tools: &[String]) -> Result<(), EditError> {
        let stage = self.stage_table_mut(name)?;
        if tools.is_empty() {
            stage.remove("available_tools");
        } else {
            set_strings(stage, "available_tools", tools);
        }
        Ok(())
    }

    /// Set one fan-out key of a stage.
    pub fn set_fan_out(&mut self, name: &str, field: FanOutField) -> Result<(), EditError> {
        let stage = self.stage_table_mut(name)?;
        match field {
            FanOutField::Worker(worker) => {
                for kind in [WorkerKind::Agent, WorkerKind::Stage, WorkerKind::Query] {
                    stage.remove(kind.key());
                }
                if let Some((kind, value)) = worker {
                    set_str(stage, kind.key(), &value);
                }
            }
            FanOutField::MergeStage(v) => {
                set_or_remove_str(stage, "merge_stage", v.as_deref().unwrap_or(""))
            }
            FanOutField::MaxWorkers(v) => set_or_remove_int(stage, "max_workers", v),
            FanOutField::MaxItems(v) => set_or_remove_int(stage, "max_items", v),
            FanOutField::OnWorkerFailure(v) => {
                set_or_remove_str(stage, "on_worker_failure", v.as_deref().unwrap_or(""))
            }
        }
        Ok(())
    }

    /// The stage's table, mutably, or [`EditError::NoSuchStage`].
    pub(super) fn stage_table_mut(
        &mut self,
        name: &str,
    ) -> Result<&mut dyn toml_edit::TableLike, EditError> {
        let item = self
            .stage_item_mut(name)
            .ok_or_else(|| EditError::NoSuchStage(name.to_string()))?;
        Ok(item.as_table_like_mut().expect("stage_item_mut checked"))
    }

    /// The stage's `transitions` table, created when missing. The stage must
    /// exist (callers check).
    pub(super) fn transitions_mut(&mut self, name: &str) -> Result<&mut Item, EditError> {
        let item = self
            .stage_item_mut(name)
            .expect("callers check the stage exists");
        ensure_child(item, "transitions")
    }

    pub(super) fn require_stage(&self, name: &str) -> Result<(), EditError> {
        if self.has_stage(name) {
            Ok(())
        } else {
            Err(EditError::NoSuchStage(name.to_string()))
        }
    }

    /// Whether the agent's `[stages]` holds `name` as any kind of value (a
    /// non-table entry still takes the key).
    pub(super) fn stages_key_taken(&self, name: &str) -> bool {
        self.doc()
            .get("stages")
            .and_then(as_table)
            .is_some_and(|t| t.contains_key(name))
    }
}

pub(super) fn set_or_remove_int(
    table: &mut dyn toml_edit::TableLike,
    key: &str,
    value: Option<u64>,
) {
    match value {
        Some(n) => set_int(table, key, clamp_i64(n)),
        None => {
            table.remove(key);
        }
    }
}

/// TOML integers are signed 64-bit; anything bigger is written as the top.
pub(super) fn clamp_i64(n: u64) -> i64 {
    n.min(i64::MAX as u64) as i64
}
