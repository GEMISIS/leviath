//! Mutators for `[[graph.edges]]`: the paths.
//!
//! The editor names a path by the stages at its two ends. A graph may hold
//! two edges between the same stages (under different names); the editor
//! shows and edits the first.

use toml_edit::{InlineTable, TableLike, Value};

use super::EditError;
use super::doc::{EdgeKind, ManifestDoc, TransformKind};
use super::order::{self, Seg, Spot};
use super::tables::{
    ensure_sub, get_str, get_strings, list_tables_mut, push_table, remove_table, set_or_remove_str,
    set_str, set_strings, set_value, sub, sub_mut,
};

/// What a custom carry does with one region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rule {
    /// Carried as it is.
    Carry,
    /// Summarized.
    Compact,
    /// Emptied.
    Clear,
}

impl Rule {
    /// The list of a custom carry the rule files a region under.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Rule::Carry => "carry",
            Rule::Compact => "compact",
            Rule::Clear => "clear",
        }
    }

    /// Every rule, in the order the editor offers them.
    pub const ALL: [Rule; 3] = [Rule::Carry, Rule::Compact, Rule::Clear];

    /// What the editor calls it.
    #[cfg(test)]
    pub(crate) fn label(self) -> &'static str {
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
    /// hint the model routes on, named after the stage it enters. Both
    /// stages must exist; a path that already exists is left as it is. In a
    /// file of `[[graph.edges]]` tables it is written after the other edges
    /// leaving the stage, or after the stage when it has none.
    pub(crate) fn add_edge(&mut self, from: &str, to: &str) -> Result<(), EditError> {
        self.require_stage(from)?;
        self.require_stage(to)?;
        if self.edge(from, to).is_some() {
            return Ok(());
        }
        let edges = self.edge_tables();
        let names: Vec<&str> = edges
            .iter()
            .filter(|e| get_str(**e, "from") == Some(from))
            .filter_map(|e| get_str(*e, "name"))
            .collect();
        let name = (1..)
            .map(|n| match n {
                1 => to.to_string(),
                n => format!("{to}-{n}"),
            })
            .find(|n| !names.contains(&n.as_str()))
            .expect("an unbounded range has a free name");
        let last_from = edges
            .iter()
            .rposition(|e| get_str(*e, "from") == Some(from));
        let count = edges.len();
        let from_index = self.stage_index(from).expect("checked just above");
        let mut edge = InlineTable::new();
        edge.insert("name", Value::from(name));
        edge.insert("from", Value::from(from));
        edge.insert("to", Value::from(to));
        edge.insert("hint", Value::from(NEW_EDGE_HINT));
        push_table(self.graph_list_mut("edges")?, edge).expect("the edges are a list");
        let anchor: Vec<Seg>;
        let spot = match last_from {
            Some(i) => {
                anchor = order::element("edges", i);
                Spot::After(&anchor)
            }
            None => {
                anchor = order::element("stages", from_index + 1);
                Spot::Before(&anchor)
            }
        };
        order::move_block(self.doc_mut(), &order::element("edges", count), spot);
        Ok(())
    }

    /// Delete a path.
    pub(crate) fn delete_edge(&mut self, from: &str, to: &str) -> Result<(), EditError> {
        self.require_stage(from)?;
        let index = self.edge_index(from, to)?;
        let edges = self
            .graph_list_mut("edges")
            .expect("edge_index found the edge in this list");
        remove_table(edges, index);
        Ok(())
    }

    /// Set when a path is taken. `Hint` drops `when` and writes a `hint`
    /// (keeping the text already there, or an empty one); `Always` drops
    /// both, since an edge with neither is always taken; every other kind
    /// writes `when` and drops `hint`.
    pub(crate) fn set_edge_kind(
        &mut self,
        from: &str,
        to: &str,
        kind: EdgeKind,
    ) -> Result<(), EditError> {
        let edge = self.edge_table_mut(from, to)?;
        match kind {
            EdgeKind::Hint => {
                edge.remove("when");
                if !edge.contains_key("hint") {
                    set_str(edge, "hint", "");
                }
            }
            EdgeKind::Always => {
                edge.remove("when");
                edge.remove("hint");
            }
            other => {
                let when = other.condition().expect("only a hint has no condition");
                set_str(edge, "when", when);
                edge.remove("hint");
            }
        }
        Ok(())
    }

    /// Set a path's hint text.
    pub(crate) fn set_edge_hint(
        &mut self,
        from: &str,
        to: &str,
        hint: &str,
    ) -> Result<(), EditError> {
        let edge = self.edge_table_mut(from, to)?;
        set_str(edge, "hint", hint);
        Ok(())
    }

    /// Require (or stop requiring) approval on a path. Turning it on adds
    /// the smallest gate there is only when the path has none, so a richer
    /// gate an author wrote survives; turning it off deletes whatever gate
    /// is there.
    pub(crate) fn set_edge_gate(
        &mut self,
        from: &str,
        to: &str,
        gated: bool,
    ) -> Result<(), EditError> {
        let edge = self.edge_table_mut(from, to)?;
        if gated {
            if !edge.contains_key("gate") {
                let mut gate = InlineTable::new();
                gate.insert("message", Value::from("Approve to continue"));
                set_value(edge, "gate", Value::InlineTable(gate));
            }
        } else {
            edge.remove("gate");
        }
        Ok(())
    }

    /// Set how context crosses a path: its `carry`. `Direct` is written as
    /// absent. Picking the kind a path already has keeps its settings; the
    /// first switch to `Custom` files every non-pinned region the leaving
    /// stage sees under `carry`.
    pub(crate) fn set_transform(
        &mut self,
        from: &str,
        to: &str,
        kind: &TransformKind,
    ) -> Result<(), EditError> {
        let current = self
            .edge(from, to)
            .ok_or_else(|| EditError::NoSuchEdge(from.to_string(), to.to_string()))?
            .transform;
        if current == *kind {
            return Ok(());
        }
        let seed: Vec<String> = self
            .effective_regions(Some(from))
            .regions
            .into_iter()
            .filter(|r| r.kind != "pinned")
            .map(|r| r.name)
            .collect();
        let edge = self
            .edge_table_mut(from, to)
            .expect("the edge was found just above");
        match kind {
            TransformKind::Direct => {
                edge.remove("carry");
            }
            TransformKind::Clear => set_str(edge, "carry", "clear"),
            TransformKind::Compact => {
                set_value(edge, "carry", variant("compact", InlineTable::new()))
            }
            TransformKind::Custom => {
                let mut rules = InlineTable::new();
                if !seed.is_empty() {
                    rules.insert("carry", Value::Array(super::tables::string_array(&seed)));
                }
                set_value(edge, "carry", variant("custom", rules));
            }
            TransformKind::Other(name) => set_str(edge, "carry", name),
        }
        Ok(())
    }

    /// File a region under exactly one of carry/compact/clear on a path,
    /// making its carry custom when it is not; emptied lists are deleted.
    pub(crate) fn set_transform_rule(
        &mut self,
        from: &str,
        to: &str,
        region: &str,
        rule: Rule,
    ) -> Result<(), EditError> {
        let edge = self.edge_table_mut(from, to)?;
        let rules = custom_rules(edge)?;
        for candidate in Rule::ALL {
            let mut kept: Vec<String> = get_strings(&*rules, candidate.key())
                .into_iter()
                .filter(|r| r != region)
                .collect();
            if candidate == rule {
                kept.push(region.to_string());
            }
            if kept.is_empty() {
                rules.remove(candidate.key());
            } else {
                set_strings(rules, candidate.key(), &kept);
            }
        }
        Ok(())
    }

    /// Set the summarizing instructions of a path: a compacting carry's
    /// `prompt`, a custom one's `compact_prompt`. Empty deletes them; any
    /// other carry turns custom to hold them.
    pub(crate) fn set_compact_prompt(
        &mut self,
        from: &str,
        to: &str,
        prompt: &str,
    ) -> Result<(), EditError> {
        let edge = self.edge_table_mut(from, to)?;
        if let Some(compact) = sub_mut(edge, "carry").and_then(|c| sub_mut(c, "compact")) {
            set_or_remove_str(compact, "prompt", prompt);
            return Ok(());
        }
        if prompt.is_empty() {
            if let Some(custom) = sub_mut(edge, "carry").and_then(|c| sub_mut(c, "custom")) {
                custom.remove("compact_prompt");
            }
            return Ok(());
        }
        set_str(custom_rules(edge)?, "compact_prompt", prompt);
        Ok(())
    }

    /// Where the first path from `from` to `to` sits in `graph.edges`.
    fn edge_index(&self, from: &str, to: &str) -> Result<usize, EditError> {
        self.edge_tables()
            .iter()
            .position(|e| get_str(*e, "from") == Some(from) && get_str(*e, "to") == Some(to))
            .ok_or_else(|| EditError::NoSuchEdge(from.to_string(), to.to_string()))
    }

    /// The first path from `from` to `to`, mutably.
    fn edge_table_mut(&mut self, from: &str, to: &str) -> Result<&mut dyn TableLike, EditError> {
        let index = self.edge_index(from, to)?;
        let edges = self
            .graph_list_mut("edges")
            .expect("edge_index found the edge in this list");
        Ok(list_tables_mut(edges)
            .into_iter()
            .nth(index)
            .expect("edge_index found it"))
    }
}

/// `{ <name> = <settings> }`: a carry variant with settings.
fn variant(name: &str, settings: InlineTable) -> Value {
    let mut t = InlineTable::new();
    t.insert(name, Value::InlineTable(settings));
    Value::InlineTable(t)
}

/// The rules table of an edge's custom carry, made custom first when it is
/// not.
fn custom_rules(edge: &mut dyn TableLike) -> Result<&mut dyn TableLike, EditError> {
    let is_custom = sub(&*edge, "carry").is_some_and(|c| c.contains_key("custom"));
    if !is_custom {
        set_value(edge, "carry", variant("custom", InlineTable::new()));
    }
    let carry = sub_mut(edge, "carry").expect("a table now");
    ensure_sub(carry, "custom")
}
