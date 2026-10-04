//! Mutators for `[blueprint]`, the graph's entry, and the stages.

use leviath_runtime::spec::graph::stage::looks_like_a_path;
use toml_edit::{Array, InlineTable, Item, TableLike, Value};

use super::doc::{ManifestDoc, StageModeView, WorkerKind, fan_out_table};
use super::order::{self, Spot};
use super::tables::{
    get_str, inline_item, insert_table, list_tables_mut, remove_table, retain_tables, set_bool,
    set_or_remove_int, set_or_remove_str, set_str, set_strings, set_value, sub_mut,
};
use super::{EditError, require_name};

/// The free-text keys of a stage the editor writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StageText {
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

/// One setting of a fan-out stage. `None` deletes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FanOutField {
    /// What the workers run, and its value: replaces the whole `worker`.
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

/// The input slots that name a stage: `{ stage_model = "plan" }` and the
/// like.
const STAGE_SLOTS: [&str; 3] = ["stage_model", "stage_max_iterations", "fan_out_max_workers"];

impl ManifestDoc {
    /// Set `[blueprint] name`.
    pub(crate) fn set_agent_name(&mut self, name: &str) -> Result<(), EditError> {
        require_name(name)?;
        set_str(self.meta_mut(), "name", name);
        Ok(())
    }

    /// Set `[blueprint] description`; empty deletes it.
    pub(crate) fn set_description(&mut self, text: &str) {
        set_or_remove_str(self.meta_mut(), "description", text);
    }

    /// Point `[graph] entry` at `stage`, which must exist.
    pub(crate) fn set_entry_stage(&mut self, stage: &str) -> Result<(), EditError> {
        self.require_stage(stage)?;
        set_str(self.graph_mut(), "entry", stage);
        Ok(())
    }

    /// Make `model` (`provider/model`) the first model every stage tries,
    /// keeping the rest of each stage's chain behind it.
    pub(crate) fn set_default_model(&mut self, model: &str) {
        for stage in self.stages() {
            let mut chain: Vec<String> = vec![model.to_string()];
            chain.extend(stage.models.into_iter().filter(|m| m != model));
            self.set_models(&stage.name, &chain)
                .expect("a listed stage exists");
        }
    }

    /// Add a stage after `after` (or at the end): autonomous, twenty tries,
    /// nowhere to go yet. In a file of `[[graph.stages]]` tables it is
    /// written after the anchor and the edges that follow it, before the
    /// next stage.
    pub(crate) fn add_stage(&mut self, name: &str, after: Option<&str>) -> Result<(), EditError> {
        require_name(name)?;
        if self.has_stage(name) {
            return Err(EditError::Taken(name.to_string()));
        }
        let at = match after {
            Some(a) => {
                self.stage_index(a)
                    .ok_or_else(|| EditError::NoSuchStage(a.to_string()))?
                    + 1
            }
            None => self.stage_names().len(),
        };
        let mut stage = InlineTable::new();
        stage.insert("name", Value::from(name));
        stage.insert("mode", Value::from("autonomous"));
        stage.insert("max_iterations", Value::from(20));
        insert_table(self.stages_list_mut(), at, stage).expect("the stages are a list");
        order::move_block(
            self.doc_mut(),
            &order::element("stages", at),
            Spot::Before(&order::element("stages", at + 1)),
        );
        Ok(())
    }

    /// Rename a stage, rewriting every edge into and out of it (and the name
    /// of an edge named after it), the entry, any fan-out naming it as worker
    /// or merge stage, and any input bound to it.
    pub(crate) fn rename_stage(&mut self, from: &str, to: &str) -> Result<(), EditError> {
        if from == to {
            return Ok(());
        }
        require_name(to)?;
        self.require_stage(from)?;
        if self.has_stage(to) {
            return Err(EditError::Taken(to.to_string()));
        }
        let stage = self.stage_table_mut(from).expect("checked just above");
        set_str(stage, "name", to);
        let taken: Vec<(String, String)> = self
            .edge_tables()
            .iter()
            .filter_map(|e| {
                Some((
                    get_str(*e, "from")?.to_string(),
                    get_str(*e, "name")?.to_string(),
                ))
            })
            .collect();
        for edge in super::doc::edge_tables_mut(self) {
            let leaves =
                get_str(&*edge, "from").map(|f| if f == from { to } else { f }.to_string());
            if get_str(&*edge, "to") == Some(from) {
                set_str(edge, "to", to);
                let free = !taken
                    .iter()
                    .any(|(f, n)| Some(f) == leaves.as_ref() && n == to);
                if get_str(&*edge, "name") == Some(from) && free {
                    set_str(edge, "name", to);
                }
            }
            if get_str(&*edge, "from") == Some(from) {
                set_str(edge, "from", to);
            }
        }
        if get_str(self.graph(), "entry") == Some(from) {
            set_str(self.graph_mut(), "entry", to);
        }
        for stage in list_tables_mut(self.stages_list_mut()) {
            let Some(fan_out) = sub_mut(stage, "mode").and_then(|m| sub_mut(m, "fan_out")) else {
                continue;
            };
            if get_str(&*fan_out, "merge_stage") == Some(from) {
                set_str(fan_out, "merge_stage", to);
            }
            if let Some(worker) = sub_mut(fan_out, "worker")
                && get_str(&*worker, "stage") == Some(from)
            {
                set_str(worker, "stage", to);
            }
        }
        self.each_binding(&mut |bind| {
            for slot in STAGE_SLOTS {
                if bind.get(slot).and_then(Value::as_str) == Some(from) {
                    bind.insert(slot, Value::from(to));
                }
            }
        });
        Ok(())
    }

    /// Delete a stage and every edge into or out of it. Refuses the last
    /// stage; deleting the entry stage points the entry at the first stage
    /// left.
    pub(crate) fn delete_stage(&mut self, name: &str) -> Result<(), EditError> {
        let index = self
            .stage_index(name)
            .ok_or_else(|| EditError::NoSuchStage(name.to_string()))?;
        let names = self.stage_names();
        let Some(first) = names.iter().find(|n| *n != name).cloned() else {
            return Err(EditError::LastStage);
        };
        remove_table(self.stages_list_mut(), index);
        if let Some(edges) = self.graph_mut().get_mut("edges") {
            retain_tables(edges, &|e| {
                get_str(e, "from") != Some(name) && get_str(e, "to") != Some(name)
            });
        }
        if get_str(self.graph(), "entry") == Some(name) {
            set_str(self.graph_mut(), "entry", &first);
        }
        Ok(())
    }

    /// Set a stage's `mode`. A fan-out needs a worker to read at all, so it
    /// starts with the first other stage as its worker (itself when it is
    /// the only one); interaction points start as `{ interactive_points = []
    /// }`. Picking the mode a stage already has keeps its settings, and
    /// picking another drops them.
    pub(crate) fn set_stage_mode(
        &mut self,
        name: &str,
        mode: &StageModeView,
    ) -> Result<(), EditError> {
        let current = self.stage(name).map(|s| s.mode);
        let worker = self
            .stage_names()
            .into_iter()
            .find(|s| s != name)
            .unwrap_or_else(|| name.to_string());
        let stage = self.stage_table_mut(name)?;
        if current.as_ref() == Some(mode) {
            return Ok(());
        }
        let value = match mode {
            StageModeView::FanOut => {
                let mut fan_out = InlineTable::new();
                fan_out.insert(
                    "worker",
                    Value::InlineTable(worker_table(WorkerKind::Stage, &worker)),
                );
                let mut t = InlineTable::new();
                t.insert("fan_out", Value::InlineTable(fan_out));
                Value::InlineTable(t)
            }
            StageModeView::InteractivePoints => {
                let mut t = InlineTable::new();
                t.insert("interactive_points", Value::Array(Array::new()));
                Value::InlineTable(t)
            }
            other => Value::from(other.as_str()),
        };
        set_value(stage, "mode", value);
        Ok(())
    }

    /// Set one of a stage's text keys; empty deletes it.
    pub(crate) fn set_stage_text(
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
    pub(crate) fn set_max_iterations(
        &mut self,
        name: &str,
        value: Option<u64>,
    ) -> Result<(), EditError> {
        let stage = self.stage_table_mut(name)?;
        set_or_remove_int(stage, "max_iterations", value.map(|n| n.max(1)));
        Ok(())
    }

    /// Set `max_revisits`; `None` deletes it.
    pub(crate) fn set_max_revisits(
        &mut self,
        name: &str,
        value: Option<u64>,
    ) -> Result<(), EditError> {
        let stage = self.stage_table_mut(name)?;
        set_or_remove_int(stage, "max_revisits", value);
        Ok(())
    }

    /// Set `allow_complete`; `None` deletes it (the runtime's default).
    pub(crate) fn set_allow_complete(
        &mut self,
        name: &str,
        value: Option<bool>,
    ) -> Result<(), EditError> {
        let stage = self.stage_table_mut(name)?;
        match value {
            Some(b) => set_bool(stage, "allow_complete", b),
            None => {
                stage.remove("allow_complete");
            }
        }
        Ok(())
    }

    /// Set a stage's model chain (`provider/model` each, or a bare model name
    /// that leaves the provider open) as `model = { models = [...] }`,
    /// keeping any other key of an existing `model` table. An empty chain
    /// deletes `model`.
    pub(crate) fn set_models(&mut self, name: &str, chain: &[String]) -> Result<(), EditError> {
        let stage = self.stage_table_mut(name)?;
        if chain.is_empty() {
            stage.remove("model");
            return Ok(());
        }
        let mut models = Array::new();
        for entry in chain {
            let mut t = InlineTable::new();
            if let Some((provider, model)) = entry.split_once('/') {
                t.insert("provider", Value::from(provider));
                t.insert("model", Value::from(model));
            } else {
                t.insert("model", Value::from(entry.as_str()));
            }
            models.push(Value::InlineTable(t));
        }
        let keeps_table = stage
            .get("model")
            .is_some_and(|m| m.as_table_like().is_some());
        if !keeps_table {
            stage.insert("model", inline_item(InlineTable::new()));
        }
        let model = sub_mut(stage, "model").expect("a table now");
        model.insert("models", Item::Value(Value::Array(models)));
        Ok(())
    }

    /// Set `tools`; an empty list deletes it.
    pub(crate) fn set_tools(&mut self, name: &str, tools: &[String]) -> Result<(), EditError> {
        let stage = self.stage_table_mut(name)?;
        set_or_remove_list(stage, "tools", tools);
        Ok(())
    }

    /// Set `connectors`, the MCP servers whose whole tool set the stage may
    /// use; an empty list deletes it.
    pub(crate) fn set_connectors(
        &mut self,
        name: &str,
        servers: &[String],
    ) -> Result<(), EditError> {
        let stage = self.stage_table_mut(name)?;
        set_or_remove_list(stage, "connectors", servers);
        Ok(())
    }

    /// Set one fan-out setting of a stage whose mode is a fan-out.
    pub(crate) fn set_fan_out(&mut self, name: &str, field: FanOutField) -> Result<(), EditError> {
        let stage = self.stage_table_mut(name)?;
        if fan_out_table(&*stage).is_none() {
            return Err(EditError::OutOfRange(format!(
                "\"{name}\" does not fan out; make it a fan-out stage first"
            )));
        }
        let fan_out = sub_mut(stage, "mode")
            .and_then(|m| sub_mut(m, "fan_out"))
            .expect("checked just above");
        match field {
            FanOutField::Worker(None) => {
                fan_out.remove("worker");
            }
            FanOutField::Worker(Some((kind, value))) => {
                set_value(
                    fan_out,
                    "worker",
                    Value::InlineTable(worker_table(kind, &value)),
                );
            }
            FanOutField::MergeStage(v) => {
                set_or_remove_str(fan_out, "merge_stage", v.as_deref().unwrap_or(""))
            }
            FanOutField::MaxWorkers(v) => set_or_remove_int(fan_out, "max_workers", v),
            FanOutField::MaxItems(v) => set_or_remove_int(fan_out, "max_items", v),
            FanOutField::OnWorkerFailure(v) => {
                set_or_remove_str(fan_out, "on_worker_failure", v.as_deref().unwrap_or(""))
            }
        }
        Ok(())
    }

    pub(super) fn require_stage(&self, name: &str) -> Result<(), EditError> {
        match self.has_stage(name) {
            true => Ok(()),
            false => Err(EditError::NoSuchStage(name.to_string())),
        }
    }

    /// The edge tables, in file order.
    pub(super) fn edge_tables(&self) -> Vec<&dyn TableLike> {
        self.graph_list("edges")
            .map(super::tables::list_tables)
            .unwrap_or_default()
    }

    /// Run `f` over every inline-table entry of every input's `binds`.
    pub(super) fn each_binding(&mut self, f: &mut dyn FnMut(&mut InlineTable)) {
        let Some(inputs) = self.graph_mut().get_mut("inputs") else {
            return;
        };
        for input in list_tables_mut(inputs) {
            let Some(binds) = input.get_mut("binds").and_then(Item::as_array_mut) else {
                continue;
            };
            for bind in binds.iter_mut().filter_map(Value::as_inline_table_mut) {
                f(bind);
            }
        }
    }
}

/// A `worker` table for what the editor's worker field holds: a stage, a
/// query, or a blueprint by name (`name` or `name@digest`) or, when the text
/// is a path, by directory.
fn worker_table(kind: WorkerKind, value: &str) -> InlineTable {
    let mut worker = InlineTable::new();
    match kind {
        WorkerKind::Stage | WorkerKind::Query => {
            worker.insert(kind.key(), Value::from(value));
        }
        WorkerKind::Agent if looks_like_a_path(value) => {
            worker.insert("blueprint_file", Value::from(value));
        }
        WorkerKind::Agent => {
            let mut blueprint = InlineTable::new();
            match value.rsplit_once('@') {
                Some((name, digest)) => {
                    blueprint.insert("name", Value::from(name));
                    blueprint.insert("digest", Value::from(digest));
                }
                None => {
                    blueprint.insert("name", Value::from(value));
                }
            }
            worker.insert(kind.key(), Value::InlineTable(blueprint));
        }
    }
    worker
}

/// Write a list of strings, or remove the key when it is empty.
pub(super) fn set_or_remove_list(table: &mut dyn TableLike, key: &str, values: &[String]) {
    if values.is_empty() {
        table.remove(key);
    } else {
        set_strings(table, key, values);
    }
}
