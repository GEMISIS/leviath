//! Mutators for what a stage takes and hands back as mime: the
//! `[stages.<name>.input]` lists, the `[stages.<name>.output]` format and
//! `[[artifacts]]` declarations, and the `[stages.<name>.tool_accepts]`
//! table that says what each tool may be handed. What a region takes
//! (`accepts`, `max_stored`) is a region key like any other and lives in
//! `regions.rs`.

use toml_edit::{Array, ArrayOfTables, InlineTable, Item, Table, TableLike, Value};

use super::doc::ManifestDoc;
use super::tables::{
    child, child_mut, ensure_parent, get_bool, get_str, get_strings, remove_and_report_empty,
    set_bool, set_or_remove_str, set_str, set_strings,
};
use super::{EditError, require_name};

/// One `[[stages.<name>.output.artifacts]]` entry as the editor shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArtifactView {
    /// `name`: what the submission calls the file.
    pub name: String,
    /// `type`: the mime type it must be, or a pattern (`video/*`).
    pub mime_type: String,
    /// `required = true`.
    pub required: bool,
    /// `description`, or empty.
    pub description: String,
}

/// Which `[stages.<name>.input]` list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputList {
    /// `accepts`: what the stage takes as parts, when its regions do not
    /// already say.
    Accepts,
    /// `as_text`: types whose parts reach the model as text whatever it
    /// takes.
    AsText,
}

impl InputList {
    fn key(self) -> &'static str {
        match self {
            InputList::Accepts => "accepts",
            InputList::AsText => "as_text",
        }
    }
}

/// One key of an artifact declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ArtifactField {
    /// `name`; refused when another artifact of the stage has it.
    Name(String),
    /// `type`; never emptied, a declaration always has one.
    Type(String),
    /// `required = true`; off deletes the key.
    Required(bool),
    /// `description`; empty deletes.
    Description(String),
}

/// The type a new artifact starts with: anything, until the author narrows
/// it.
pub(crate) const NEW_ARTIFACT_TYPE: &str = "*/*";

/// A typed list of mime type patterns: commas or spaces between them,
/// lowercased, blanks and repeats dropped.
pub(crate) fn split_list(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in text.split([',', ' ', '\t']) {
        let item = item.trim().to_ascii_lowercase();
        if !item.is_empty() && !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

/// `[stages.<name>.output] format`, or empty.
pub(super) fn output_format_of(stage: &Item) -> String {
    child(stage, "output")
        .and_then(|output| get_str(output, "format"))
        .unwrap_or_default()
        .to_string()
}

/// `[stages.<name>.tool_accepts]`: each tool and what it may be handed, in
/// document order. An entry that is not a list of strings is left out (and
/// left alone).
pub(super) fn tool_limits_of(stage: &Item) -> Vec<(String, Vec<String>)> {
    child(stage, "tool_accepts")
        .map(|limits| {
            limits
                .iter()
                .filter(|(_, v)| v.as_array().is_some())
                .map(|(tool, _)| (tool.to_string(), get_strings(limits, tool)))
                .collect()
        })
        .unwrap_or_default()
}

/// The keys of the manifest's own `[mime_types]` rows, as written.
pub(crate) fn mime_type_keys(doc: &ManifestDoc) -> Vec<String> {
    doc.doc()
        .get("mime_types")
        .and_then(Item::as_table_like)
        .map(|rows| rows.iter().map(|(key, _)| key.to_string()).collect())
        .unwrap_or_default()
}

/// The `[stages.<name>.input]` list `which`, or empty.
pub(super) fn input_list(stage: &Item, which: InputList) -> Vec<String> {
    child(stage, "input")
        .map(|input| get_strings(input, which.key()))
        .unwrap_or_default()
}

/// The artifacts a stage declares, in order. Both shapes of list are read:
/// `[[...artifacts]]` tables and an inline `artifacts = [{ ... }]`.
pub(super) fn artifacts_of(stage: &Item) -> Vec<ArtifactView> {
    child(stage, "output")
        .and_then(|output| output.get("artifacts"))
        .map(|list| {
            artifact_tables(list)
                .into_iter()
                .map(artifact_view)
                .collect()
        })
        .unwrap_or_default()
}

/// The tables of an artifact list, whichever shape it has; nothing for a
/// value that is not a list of tables.
fn artifact_tables(list: &Item) -> Vec<&dyn TableLike> {
    if let Some(tables) = list.as_array_of_tables() {
        return tables.iter().map(|t| t as &dyn TableLike).collect();
    }
    list.as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_inline_table)
                .map(|t| t as &dyn TableLike)
                .collect()
        })
        .unwrap_or_default()
}

/// [`artifact_tables`], mutably.
fn artifact_tables_mut(list: &mut Item) -> Vec<&mut dyn TableLike> {
    match list {
        Item::ArrayOfTables(tables) => tables.iter_mut().map(|t| t as &mut dyn TableLike).collect(),
        Item::Value(Value::Array(array)) => array
            .iter_mut()
            .filter_map(Value::as_inline_table_mut)
            .map(|t| t as &mut dyn TableLike)
            .collect(),
        _ => Vec::new(),
    }
}

fn artifact_view(table: &dyn TableLike) -> ArtifactView {
    ArtifactView {
        name: get_str(table, "name").unwrap_or_default().to_string(),
        mime_type: get_str(table, "type").unwrap_or_default().to_string(),
        required: get_bool(table, "required") == Some(true),
        description: get_str(table, "description")
            .unwrap_or_default()
            .to_string(),
    }
}

/// Append a `{ name, type = "*/*" }` entry to a list, in the list's shape.
fn push_artifact(list: &mut Item, name: &str) -> Result<(), EditError> {
    if let Some(tables) = list.as_array_of_tables_mut() {
        let mut table = Table::new();
        table.insert("name", Item::Value(Value::from(name)));
        table.insert("type", Item::Value(Value::from(NEW_ARTIFACT_TYPE)));
        tables.push(table);
        return Ok(());
    }
    let Some(array) = list.as_array_mut() else {
        return Err(EditError::NotATable("artifacts".to_string()));
    };
    let mut table = InlineTable::new();
    table.insert("name", Value::from(name));
    table.insert("type", Value::from(NEW_ARTIFACT_TYPE));
    array.push(Value::InlineTable(table));
    Ok(())
}

/// Drop the `index`th table of a list; `false` when there is none. An
/// inline list is counted by its tables, the way it is read, so a stray
/// value in it neither shifts the count nor gets removed in a table's
/// place.
fn remove_artifact(list: &mut Item, index: usize) -> bool {
    match list {
        Item::ArrayOfTables(tables) if index < tables.len() => {
            tables.remove(index);
            true
        }
        Item::Value(Value::Array(array)) => {
            let raw = array
                .iter()
                .enumerate()
                .filter(|(_, v)| v.is_inline_table())
                .map(|(i, _)| i)
                .nth(index);
            match raw {
                Some(i) => {
                    array.remove(i);
                    true
                }
                None => false,
            }
        }
        _ => false,
    }
}

fn no_artifact(index: usize) -> EditError {
    EditError::OutOfRange(format!("there is no artifact {}", index + 1))
}

impl ManifestDoc {
    /// The artifacts a stage declares; empty for a stage that is not there.
    pub(crate) fn artifacts(&self, stage: &str) -> Vec<ArtifactView> {
        self.stage_item(stage).map(artifacts_of).unwrap_or_default()
    }

    /// Write one `[stages.<name>.input]` list. An empty list deletes the
    /// key, and the `input` table with it when nothing else is left there.
    pub(crate) fn set_stage_input(
        &mut self,
        stage: &str,
        which: InputList,
        values: &[String],
    ) -> Result<(), EditError> {
        let stage_item = self
            .stage_item_mut(stage)
            .ok_or_else(|| EditError::NoSuchStage(stage.to_string()))?;
        if values.is_empty() {
            if let Some(input) = child_mut(stage_item, "input")
                && remove_and_report_empty(
                    input.as_table_like_mut().expect("child_mut checked"),
                    which.key(),
                )
            {
                stage_item
                    .as_table_like_mut()
                    .expect("a stage is a table")
                    .remove("input");
            }
            return Ok(());
        }
        let input = ensure_parent(stage_item, "input")?;
        set_strings(
            input.as_table_like_mut().expect("ensure_parent checked"),
            which.key(),
            values,
        );
        Ok(())
    }

    /// Set `[stages.<name>.output] format`; empty deletes it, and the
    /// `output` table with it when nothing else is left there.
    pub(crate) fn set_output_format(&mut self, stage: &str, format: &str) -> Result<(), EditError> {
        let stage_item = self
            .stage_item_mut(stage)
            .ok_or_else(|| EditError::NoSuchStage(stage.to_string()))?;
        if format.is_empty() {
            if let Some(output) = child_mut(stage_item, "output")
                && remove_and_report_empty(
                    output.as_table_like_mut().expect("child_mut checked"),
                    "format",
                )
            {
                stage_item
                    .as_table_like_mut()
                    .expect("a stage is a table")
                    .remove("output");
            }
            return Ok(());
        }
        let output = ensure_parent(stage_item, "output")?;
        set_str(
            output.as_table_like_mut().expect("ensure_parent checked"),
            "format",
            format,
        );
        Ok(())
    }

    /// Set what `tool` may be handed at the stage (`tool_accepts`); an empty
    /// list lifts the limit, and takes the table with it when it was the
    /// last one.
    pub(crate) fn set_tool_accepts(
        &mut self,
        stage: &str,
        tool: &str,
        types: &[String],
    ) -> Result<(), EditError> {
        require_name(tool)?;
        let stage_item = self
            .stage_item_mut(stage)
            .ok_or_else(|| EditError::NoSuchStage(stage.to_string()))?;
        if types.is_empty() {
            if let Some(limits) = child_mut(stage_item, "tool_accepts")
                && remove_and_report_empty(
                    limits.as_table_like_mut().expect("child_mut checked"),
                    tool,
                )
            {
                stage_item
                    .as_table_like_mut()
                    .expect("a stage is a table")
                    .remove("tool_accepts");
            }
            return Ok(());
        }
        let limits = ensure_parent(stage_item, "tool_accepts")?;
        set_strings(
            limits.as_table_like_mut().expect("ensure_parent checked"),
            tool,
            types,
        );
        Ok(())
    }

    /// Declare a file the stage hands back: a new artifact with the name,
    /// taking any type until the author narrows it. Refuses a name outside
    /// the runtime's charset or one the stage already declares.
    pub(crate) fn add_artifact(&mut self, stage: &str, name: &str) -> Result<(), EditError> {
        require_name(name)?;
        if self.artifacts(stage).iter().any(|a| a.name == name) {
            return Err(EditError::Taken(name.to_string()));
        }
        let stage_item = self
            .stage_item_mut(stage)
            .ok_or_else(|| EditError::NoSuchStage(stage.to_string()))?;
        let output = ensure_parent(stage_item, "output")?;
        let inline = output.is_inline_table();
        let table = output.as_table_like_mut().expect("ensure_parent checked");
        if let Some(list) = table.get_mut("artifacts") {
            return push_artifact(list, name);
        }
        // A fresh list in the parent's shape: `[[stages.x.output.artifacts]]`
        // under a headed stage, `artifacts = [{ ... }]` under an inline one.
        let mut list = if inline {
            Item::Value(Value::Array(Array::new()))
        } else {
            Item::ArrayOfTables(ArrayOfTables::new())
        };
        push_artifact(&mut list, name).expect("a fresh list is a list");
        table.insert("artifacts", list);
        Ok(())
    }

    /// Change one key of the stage's `index`th artifact.
    pub(crate) fn set_artifact(
        &mut self,
        stage: &str,
        index: usize,
        field: ArtifactField,
    ) -> Result<(), EditError> {
        if let ArtifactField::Name(name) = &field {
            require_name(name)?;
            let taken = self
                .artifacts(stage)
                .iter()
                .enumerate()
                .any(|(i, a)| i != index && a.name == *name);
            if taken {
                return Err(EditError::Taken(name.clone()));
            }
        }
        let table = self.artifact_table_mut(stage, index)?;
        match field {
            ArtifactField::Name(name) => set_str(table, "name", &name),
            ArtifactField::Type(mime_type) => {
                if mime_type.is_empty() {
                    return Err(EditError::OutOfRange(
                        "an artifact needs a type, or a pattern such as image/*".to_string(),
                    ));
                }
                set_str(table, "type", &mime_type);
            }
            ArtifactField::Required(on) => {
                if on {
                    set_bool(table, "required", true);
                } else {
                    table.remove("required");
                }
            }
            ArtifactField::Description(text) => set_or_remove_str(table, "description", &text),
        }
        Ok(())
    }

    /// Drop the stage's `index`th artifact, and the emptied list and
    /// `output` table with it.
    pub(crate) fn delete_artifact(&mut self, stage: &str, index: usize) -> Result<(), EditError> {
        let stage_item = self
            .stage_item_mut(stage)
            .ok_or_else(|| EditError::NoSuchStage(stage.to_string()))?;
        let Some(output) = child_mut(stage_item, "output") else {
            return Err(no_artifact(index));
        };
        let table = output.as_table_like_mut().expect("child_mut checked");
        let Some(list) = table.get_mut("artifacts") else {
            return Err(no_artifact(index));
        };
        if !remove_artifact(list, index) {
            return Err(no_artifact(index));
        }
        if artifact_tables(list).is_empty() && remove_and_report_empty(table, "artifacts") {
            stage_item
                .as_table_like_mut()
                .expect("a stage is a table")
                .remove("output");
        }
        Ok(())
    }

    /// The `index`th artifact's table, mutably.
    fn artifact_table_mut(
        &mut self,
        stage: &str,
        index: usize,
    ) -> Result<&mut dyn TableLike, EditError> {
        let stage_item = self
            .stage_item_mut(stage)
            .ok_or_else(|| EditError::NoSuchStage(stage.to_string()))?;
        child_mut(stage_item, "output")
            .and_then(|output| {
                output
                    .as_table_like_mut()
                    .expect("child_mut checked")
                    .get_mut("artifacts")
            })
            .and_then(|list| artifact_tables_mut(list).into_iter().nth(index))
            .ok_or_else(|| no_artifact(index))
    }
}
