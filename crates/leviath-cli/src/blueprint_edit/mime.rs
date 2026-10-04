//! Mutators for what a stage takes and hands back as mime: its
//! `input_accepts` and `input_as_text` lists, its `output` format and
//! `artifacts`, and its `tool_accepts` table that says what each tool may be
//! handed. What a region takes (`accepts`) is a region setting like any
//! other and lives in `regions.rs`.

use toml_edit::{Array, InlineTable, Item, TableLike, Value};

use super::doc::ManifestDoc;
use super::tables::{
    ensure_sub, get_bool, get_str, get_strings, list_tables, list_tables_mut, push_table,
    remove_and_report_empty, remove_table, set_bool, set_or_remove_str, set_str, set_strings, sub,
    sub_mut,
};
use super::{EditError, require_name};

/// One `output.artifacts` entry as the editor shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArtifactView {
    /// `name`: what the submission calls the file.
    pub name: String,
    /// `mime_type`: the type it must be, or a pattern (`video/*`).
    pub mime_type: String,
    /// `required = true`.
    pub required: bool,
    /// `description`, or empty.
    pub description: String,
}

/// Which of a stage's input lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputList {
    /// `input_accepts`: what the stage takes as parts, when its regions do
    /// not already say.
    Accepts,
    /// `input_as_text`: types whose parts reach the model as text whatever
    /// it takes.
    AsText,
}

impl InputList {
    fn key(self) -> &'static str {
        match self {
            InputList::Accepts => "input_accepts",
            InputList::AsText => "input_as_text",
        }
    }
}

/// One setting of an artifact declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ArtifactField {
    /// `name`; refused when another artifact of the stage has it.
    Name(String),
    /// `mime_type`; never emptied, a declaration always has one.
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

/// A stage's `tool_accepts`: each tool and what it may be handed, in
/// document order. An entry that is not a list of strings is left out (and
/// left alone).
pub(super) fn tool_limits_of(stage: &dyn TableLike) -> Vec<(String, Vec<String>)> {
    sub(stage, "tool_accepts")
        .map(|limits| {
            limits
                .iter()
                .filter(|(_, v)| v.as_array().is_some())
                .map(|(tool, _)| (tool.to_string(), get_strings(limits, tool)))
                .collect()
        })
        .unwrap_or_default()
}

/// The patterns of the graph's own `mime_types` rows, as written.
pub(crate) fn mime_type_keys(doc: &ManifestDoc) -> Vec<String> {
    sub(doc.graph(), "mime_types")
        .map(|rows| rows.iter().map(|(key, _)| key.to_string()).collect())
        .unwrap_or_default()
}

/// The artifacts a stage declares, in order. Both shapes of list are read:
/// `[[...artifacts]]` tables and an inline `artifacts = [{ ... }]`.
pub(super) fn artifacts_of(stage: &dyn TableLike) -> Vec<ArtifactView> {
    sub(stage, "output")
        .and_then(|output| output.get("artifacts"))
        .map(|list| list_tables(list).into_iter().map(artifact_view).collect())
        .unwrap_or_default()
}

fn artifact_view(table: &dyn TableLike) -> ArtifactView {
    ArtifactView {
        name: get_str(table, "name").unwrap_or_default().to_string(),
        mime_type: get_str(table, "mime_type").unwrap_or_default().to_string(),
        required: get_bool(table, "required") == Some(true),
        description: get_str(table, "description")
            .unwrap_or_default()
            .to_string(),
    }
}

fn no_artifact(index: usize) -> EditError {
    EditError::OutOfRange(format!("there is no artifact {}", index + 1))
}

impl ManifestDoc {
    /// The artifacts a stage declares; empty for a stage that is not there.
    pub(crate) fn artifacts(&self, stage: &str) -> Vec<ArtifactView> {
        self.stage_table(stage)
            .map(artifacts_of)
            .unwrap_or_default()
    }

    /// Write one of a stage's input lists. An empty list deletes the key.
    pub(crate) fn set_stage_input(
        &mut self,
        stage: &str,
        which: InputList,
        values: &[String],
    ) -> Result<(), EditError> {
        let table = self.stage_table_mut(stage)?;
        super::stages::set_or_remove_list(table, which.key(), values);
        Ok(())
    }

    /// Set a stage's `output.format`; empty deletes it, and the `output`
    /// table with it when nothing else is left there.
    pub(crate) fn set_output_format(&mut self, stage: &str, format: &str) -> Result<(), EditError> {
        let table = self.stage_table_mut(stage)?;
        if format.is_empty() {
            if let Some(output) = sub_mut(table, "output")
                && remove_and_report_empty(output, "format")
            {
                table.remove("output");
            }
            return Ok(());
        }
        let output = ensure_sub(table, "output")?;
        set_str(output, "format", format);
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
        let table = self.stage_table_mut(stage)?;
        if types.is_empty() {
            if let Some(limits) = sub_mut(table, "tool_accepts")
                && remove_and_report_empty(limits, tool)
            {
                table.remove("tool_accepts");
            }
            return Ok(());
        }
        let limits = ensure_sub(table, "tool_accepts")?;
        set_strings(limits, tool, types);
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
        let table = self.stage_table_mut(stage)?;
        let output = ensure_sub(table, "output")?;
        if !output.contains_key("artifacts") {
            output.insert("artifacts", Item::Value(Value::Array(Array::new())));
        }
        let list = output
            .get_mut("artifacts")
            .expect("present or inserted just above");
        let mut entry = InlineTable::new();
        entry.insert("name", Value::from(name));
        entry.insert("mime_type", Value::from(NEW_ARTIFACT_TYPE));
        push_table(list, entry)
    }

    /// Change one setting of the stage's `index`th artifact.
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
                set_str(table, "mime_type", &mime_type);
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
        let table = self.stage_table_mut(stage)?;
        let Some(output) = sub_mut(table, "output") else {
            return Err(no_artifact(index));
        };
        let Some(list) = output.get_mut("artifacts") else {
            return Err(no_artifact(index));
        };
        if !remove_table(list, index) {
            return Err(no_artifact(index));
        }
        if list_tables(list).is_empty() && remove_and_report_empty(output, "artifacts") {
            table.remove("output");
        }
        Ok(())
    }

    /// The `index`th artifact's table, mutably.
    fn artifact_table_mut(
        &mut self,
        stage: &str,
        index: usize,
    ) -> Result<&mut dyn TableLike, EditError> {
        let table = self.stage_table_mut(stage)?;
        sub_mut(table, "output")
            .and_then(|output| output.get_mut("artifacts"))
            .and_then(|list| list_tables_mut(list).into_iter().nth(index))
            .ok_or_else(|| no_artifact(index))
    }
}
