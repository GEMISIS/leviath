//! Mutators for `[stages.<from>.transitions.<to>]`: the paths.

use toml_edit::{InlineTable, Item, Value};

use super::EditError;
use super::doc::{EdgeKind, ManifestDoc, TransformKind};
use super::tables::{
    child_mut, ensure_child, get_strings, new_table, set_or_remove_str, set_str, set_strings,
};

/// What a custom transform does with one region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// Carried as it is.
    Carry,
    /// Summarized.
    Compact,
    /// Dropped.
    Clear,
}

impl Rule {
    /// The `transform_config` list the rule files a region under.
    pub fn key(self) -> &'static str {
        match self {
            Rule::Carry => "carry",
            Rule::Compact => "compact",
            Rule::Clear => "clear",
        }
    }

    /// Every rule, in the order the editor offers them.
    pub const ALL: [Rule; 3] = [Rule::Carry, Rule::Compact, Rule::Clear];

    /// What the editor calls it.
    pub fn label(self) -> &'static str {
        match self {
            Rule::Carry => "Carry",
            Rule::Compact => "Summarize",
            Rule::Clear => "Drop",
        }
    }
}

/// The hint a path starts life with.
pub const NEW_EDGE_HINT: &str = "Continue here when appropriate";

impl ManifestDoc {
    /// Add a path from one stage to another (or to itself: a self-loop) as a
    /// hint the model routes on. Both stages must exist; a path that already
    /// exists is left as it is.
    pub fn add_edge(&mut self, from: &str, to: &str) -> Result<(), EditError> {
        self.require_stage(from)?;
        self.require_stage(to)?;
        let transitions = self.transitions_mut(from)?;
        let inline = transitions.is_inline_table();
        let table = transitions
            .as_table_like_mut()
            .expect("transitions_mut checked");
        if table.contains_key(to) {
            return Ok(());
        }
        let mut edge = new_table(inline);
        set_str(
            edge.as_table_like_mut().expect("just built"),
            "hint",
            NEW_EDGE_HINT,
        );
        table.insert(to, edge);
        Ok(())
    }

    /// Delete a path.
    pub fn delete_edge(&mut self, from: &str, to: &str) -> Result<(), EditError> {
        let stage = self
            .stage_item_mut(from)
            .ok_or_else(|| EditError::NoSuchStage(from.to_string()))?;
        let removed = child_mut(stage, "transitions")
            .and_then(|t| t.as_table_like_mut().expect("child_mut checked").remove(to));
        if removed.is_none() {
            return Err(EditError::NoSuchEdge(from.to_string(), to.to_string()));
        }
        Ok(())
    }

    /// Set when a path is taken. `Hint` writes a `hint` (keeping the text
    /// already there, or an empty one) and drops `condition`; every other
    /// kind writes `condition` and drops `hint`.
    pub fn set_edge_kind(&mut self, from: &str, to: &str, kind: EdgeKind) -> Result<(), EditError> {
        let edge = self.edge_table_mut(from, to)?;
        match kind.condition() {
            None => {
                edge.remove("condition");
                if !edge.contains_key("hint") {
                    set_str(edge, "hint", "");
                }
            }
            Some(condition) => {
                set_str(edge, "condition", condition);
                edge.remove("hint");
            }
        }
        Ok(())
    }

    /// Set a path's hint text.
    pub fn set_edge_hint(&mut self, from: &str, to: &str, hint: &str) -> Result<(), EditError> {
        let edge = self.edge_table_mut(from, to)?;
        set_str(edge, "hint", hint);
        Ok(())
    }

    /// Require (or stop requiring) approval on a path. Turning it on adds
    /// the smallest gate there is only when the path has none, so a richer
    /// gate an author wrote survives; turning it off deletes whatever gate
    /// is there.
    pub fn set_edge_gate(&mut self, from: &str, to: &str, gated: bool) -> Result<(), EditError> {
        let edge = self.edge_table_mut(from, to)?;
        if gated {
            if !edge.contains_key("gate") {
                let mut gate = InlineTable::new();
                gate.insert("message", Value::from("Approve to continue"));
                edge.insert("gate", Item::Value(Value::InlineTable(gate)));
            }
        } else {
            edge.remove("gate");
        }
        Ok(())
    }

    /// Set how context crosses a path. `Direct` is written as absent. The
    /// first switch to `Custom` seeds `transform_config` with every
    /// non-pinned region the leaving stage sees filed under `carry`; the
    /// config is otherwise left alone, so switching away and back costs
    /// nothing.
    pub fn set_transform(
        &mut self,
        from: &str,
        to: &str,
        kind: &TransformKind,
    ) -> Result<(), EditError> {
        let seed: Option<Vec<String>> = if *kind == TransformKind::Custom
            && self.edge(from, to).is_some_and(|e| !e.rules.present)
        {
            Some(
                self.effective_regions(Some(from))
                    .regions
                    .into_iter()
                    .filter(|r| r.kind != "pinned")
                    .map(|r| r.name)
                    .collect(),
            )
        } else {
            None
        };
        let item = self.edge_item_mut(from, to)?;
        let spelling = match kind {
            TransformKind::Direct => "",
            other => other.as_str(),
        };
        set_or_remove_str(
            item.as_table_like_mut().expect("edge_item_mut checked"),
            "transform",
            spelling,
        );
        if let Some(carry) = seed
            && !carry.is_empty()
        {
            let config = ensure_child(item, "transform_config")?;
            let table = config.as_table_like_mut().expect("ensure_child checked");
            set_strings(table, "carry", &carry);
        }
        Ok(())
    }

    /// File a region under exactly one of carry/compact/clear on a path,
    /// creating `transform_config` when missing; emptied lists are deleted.
    pub fn set_transform_rule(
        &mut self,
        from: &str,
        to: &str,
        region: &str,
        rule: Rule,
    ) -> Result<(), EditError> {
        let item = self.edge_item_mut(from, to)?;
        let config = ensure_child(item, "transform_config")?;
        let table = config.as_table_like_mut().expect("ensure_child checked");
        for candidate in Rule::ALL {
            let mut kept: Vec<String> = get_strings(table, candidate.key())
                .into_iter()
                .filter(|r| r != region)
                .collect();
            if candidate == rule {
                kept.push(region.to_string());
            }
            if kept.is_empty() {
                table.remove(candidate.key());
            } else {
                set_strings(table, candidate.key(), &kept);
            }
        }
        Ok(())
    }

    /// Set the custom transform's summarizing instructions; empty deletes
    /// them (and never creates the config just to hold nothing).
    pub fn set_compact_prompt(
        &mut self,
        from: &str,
        to: &str,
        prompt: &str,
    ) -> Result<(), EditError> {
        let item = self.edge_item_mut(from, to)?;
        if prompt.is_empty() {
            if let Some(config) = child_mut(item, "transform_config") {
                config
                    .as_table_like_mut()
                    .expect("child_mut checked")
                    .remove("compact_prompt");
            }
            return Ok(());
        }
        let config = ensure_child(item, "transform_config")?;
        set_str(
            config.as_table_like_mut().expect("ensure_child checked"),
            "compact_prompt",
            prompt,
        );
        Ok(())
    }

    /// The path's item, or [`EditError::NoSuchEdge`].
    fn edge_item_mut(&mut self, from: &str, to: &str) -> Result<&mut Item, EditError> {
        let missing = || EditError::NoSuchEdge(from.to_string(), to.to_string());
        let stage = self.stage_item_mut(from).ok_or_else(missing)?;
        let transitions = child_mut(stage, "transitions").ok_or_else(missing)?;
        child_mut(transitions, to).ok_or_else(missing)
    }

    fn edge_table_mut(
        &mut self,
        from: &str,
        to: &str,
    ) -> Result<&mut dyn toml_edit::TableLike, EditError> {
        Ok(self
            .edge_item_mut(from, to)?
            .as_table_like_mut()
            .expect("edge_item_mut checked"))
    }
}
