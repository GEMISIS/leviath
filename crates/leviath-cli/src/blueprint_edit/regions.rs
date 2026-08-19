//! Mutators for context regions (`[context.regions]` and a stage's own
//! `[stages.<name>.context.regions]`) and for a stage's `tool_routing`.
//!
//! Both layouts have the same table schema, so one set of internals serves
//! both; [`RegionScope`] says which.

use toml_edit::{InlineTable, Item, Value};

use super::doc::ManifestDoc;
use super::stages::set_or_remove_int;
use super::tables::{
    child_mut, ensure_child, get_str, remove_and_report_empty, rename_key, set_bool,
    set_or_remove_str, set_str,
};
use super::{EditError, require_name};

/// Whose regions: the agent's shared layout, or one stage's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegionScope {
    /// `[context.regions]`.
    Shared,
    /// `[stages.<name>.context.regions]`.
    Stage(String),
}

impl RegionScope {
    /// The stage name, for [`ManifestDoc::regions`] and friends.
    pub fn stage(&self) -> Option<&str> {
        match self {
            RegionScope::Shared => None,
            RegionScope::Stage(s) => Some(s),
        }
    }
}

/// The per-region keys the editor writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionField {
    /// `kind` (never deleted: a region always has one).
    Kind,
    /// `budget = "N%"`, 0 to 100.
    BudgetPercent,
    /// `max_tokens`, at least 1.
    MaxTokens,
    /// `required = true`; off deletes the key.
    Required,
    /// `required_message`.
    RequiredMessage,
    /// A string `seed`. A table-shaped seed is never touched.
    Seed,
    /// `max_items`, at least 1.
    MaxItems,
    /// `strategy`.
    Strategy,
    /// `overflow`, at least 1.
    Overflow,
    /// `description`.
    Description,
}

/// A value for a [`RegionField`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegionValue {
    /// For the text keys; empty deletes.
    Text(String),
    /// For the numeric keys; `None` deletes.
    Number(Option<u64>),
    /// For `required`.
    Flag(bool),
}

impl ManifestDoc {
    /// Add a region to a layout with starter values (`pinned`, `5%`, `4000`
    /// tokens), creating `[context.regions]` when the scope has none.
    pub fn add_region(&mut self, scope: &RegionScope, name: &str) -> Result<(), EditError> {
        require_name(name)?;
        let regions = self.regions_item_ensure(scope)?;
        let table = regions
            .as_table_like_mut()
            .expect("regions_item_ensure checked");
        if table.contains_key(name) {
            return Err(EditError::Taken(name.to_string()));
        }
        let mut region = InlineTable::new();
        region.insert("kind", Value::from("pinned"));
        region.insert("budget", Value::from("5%"));
        region.insert("max_tokens", Value::from(4000));
        // Inline whichever shape the parent has: a headed `[context.regions]`
        // lists its regions as one-line inline tables, the way every bundled
        // agent writes them.
        table.insert(name, Item::Value(Value::InlineTable(region)));
        Ok(())
    }

    /// Rename a region, rewriting the tool routing that named it: every
    /// stage's for a shared region (except stages with their own layout,
    /// whose routing names their own regions), that stage's for its own.
    pub fn rename_region(
        &mut self,
        scope: &RegionScope,
        from: &str,
        to: &str,
    ) -> Result<(), EditError> {
        if from == to {
            return Ok(());
        }
        require_name(to)?;
        let regions = self
            .regions_item_mut(scope)
            .ok_or_else(|| EditError::NoSuchRegion(from.to_string()))?;
        let table = regions
            .as_table_like_mut()
            .expect("regions_item_mut checked");
        if !table.contains_key(from) {
            return Err(EditError::NoSuchRegion(from.to_string()));
        }
        if table.contains_key(to) {
            return Err(EditError::Taken(to.to_string()));
        }
        rename_key(table, from, to);
        self.retarget_routing(scope, from, Some(to));
        Ok(())
    }

    /// Delete a region, and stop routing tool results into it wherever the
    /// scope's routing did.
    pub fn delete_region(&mut self, scope: &RegionScope, name: &str) -> Result<(), EditError> {
        let regions = self
            .regions_item_mut(scope)
            .ok_or_else(|| EditError::NoSuchRegion(name.to_string()))?;
        let table = regions
            .as_table_like_mut()
            .expect("regions_item_mut checked");
        if table.remove(name).is_none() {
            return Err(EditError::NoSuchRegion(name.to_string()));
        }
        self.retarget_routing(scope, name, None);
        Ok(())
    }

    /// Write one key of a region.
    pub fn set_region_field(
        &mut self,
        scope: &RegionScope,
        name: &str,
        field: RegionField,
        value: RegionValue,
    ) -> Result<(), EditError> {
        let regions = self
            .regions_item_mut(scope)
            .ok_or_else(|| EditError::NoSuchRegion(name.to_string()))?;
        let region =
            child_mut(regions, name).ok_or_else(|| EditError::NoSuchRegion(name.to_string()))?;
        let table = region.as_table_like_mut().expect("child_mut checked");
        match (field, value) {
            (RegionField::Kind, RegionValue::Text(kind)) => {
                if !kind.is_empty() {
                    set_str(table, "kind", &kind);
                }
            }
            (RegionField::BudgetPercent, RegionValue::Number(Some(pct))) => {
                set_str(table, "budget", &format!("{}%", pct.min(100)));
            }
            (RegionField::BudgetPercent, RegionValue::Number(None)) => {
                table.remove("budget");
            }
            (RegionField::MaxTokens, RegionValue::Number(n)) => {
                set_or_remove_int(table, "max_tokens", n.map(|n| n.max(1)));
            }
            (RegionField::MaxItems, RegionValue::Number(n)) => {
                set_or_remove_int(table, "max_items", n.map(|n| n.max(1)));
            }
            (RegionField::Overflow, RegionValue::Number(n)) => {
                set_or_remove_int(table, "overflow", n.map(|n| n.max(1)));
            }
            (RegionField::Required, RegionValue::Flag(on)) => {
                if on {
                    set_bool(table, "required", true);
                } else {
                    table.remove("required");
                }
            }
            (RegionField::RequiredMessage, RegionValue::Text(t)) => {
                set_or_remove_str(table, "required_message", &t);
            }
            (RegionField::Strategy, RegionValue::Text(t)) => {
                set_or_remove_str(table, "strategy", &t)
            }
            (RegionField::Description, RegionValue::Text(t)) => {
                set_or_remove_str(table, "description", &t);
            }
            (RegionField::Seed, RegionValue::Text(t)) => {
                let is_table = table
                    .get("seed")
                    .is_some_and(|s| s.as_table_like().is_some());
                if !t.is_empty() {
                    set_str(table, "seed", &t);
                } else if !is_table {
                    table.remove("seed");
                }
            }
            (field, value) => {
                return Err(EditError::OutOfRange(format!(
                    "{field:?} does not take {value:?}"
                )));
            }
        }
        Ok(())
    }

    /// Give a stage its own layout: a deep copy of the shared regions as they
    /// are now, so it starts from what it inherited. A stage that already
    /// has one is left alone.
    pub fn create_stage_override(&mut self, stage: &str) -> Result<(), EditError> {
        self.require_stage(stage)?;
        if self.regions_item(Some(stage)).is_some() {
            return Ok(());
        }
        let shared: Option<Item> = self.regions_item(None).cloned();
        let stage_item = self.stage_item_mut(stage).expect("require_stage checked");
        let context = ensure_child(stage_item, "context")?;
        let inline = context.is_inline_table();
        let regions = match shared {
            Some(item) if !inline => item,
            Some(item) => {
                // An inline `context = {}` needs an inline copy.
                Item::Value(item.into_value().expect("a table converts to a value"))
            }
            None => super::tables::new_table(inline),
        };
        context
            .as_table_like_mut()
            .expect("ensure_child checked")
            .insert("regions", regions);
        Ok(())
    }

    /// Drop a stage's own layout, `[stages.<name>.context]` and everything in
    /// it, so it inherits the shared regions again.
    pub fn remove_stage_override(&mut self, stage: &str) -> Result<(), EditError> {
        let table = self.stage_table_mut(stage)?;
        table.remove("context");
        Ok(())
    }

    /// Set a stage's default region for tool results; empty removes it (and
    /// the `tool_routing` table when nothing else keeps it).
    pub fn set_tool_routing_default(&mut self, stage: &str, region: &str) -> Result<(), EditError> {
        let stage_item = self
            .stage_item_mut(stage)
            .ok_or_else(|| EditError::NoSuchStage(stage.to_string()))?;
        if region.is_empty() {
            if let Some(routing) = child_mut(stage_item, "tool_routing")
                && remove_and_report_empty(
                    routing.as_table_like_mut().expect("child_mut checked"),
                    "default_region",
                )
            {
                stage_item
                    .as_table_like_mut()
                    .expect("a stage is a table")
                    .remove("tool_routing");
            }
            return Ok(());
        }
        let routing = ensure_child(stage_item, "tool_routing")?;
        set_str(
            routing.as_table_like_mut().expect("ensure_child checked"),
            "default_region",
            region,
        );
        Ok(())
    }

    /// Route one tool's results to a region; an empty region stops routing
    /// it, tidying emptied `overrides`/`tool_routing` tables away.
    pub fn set_tool_routing_override(
        &mut self,
        stage: &str,
        tool: &str,
        region: &str,
    ) -> Result<(), EditError> {
        let stage_item = self
            .stage_item_mut(stage)
            .ok_or_else(|| EditError::NoSuchStage(stage.to_string()))?;
        if region.is_empty() {
            let Some(routing) = child_mut(stage_item, "tool_routing") else {
                return Ok(());
            };
            if let Some(overrides) = child_mut(routing, "overrides")
                && remove_and_report_empty(
                    overrides.as_table_like_mut().expect("child_mut checked"),
                    tool,
                )
                && remove_and_report_empty(
                    routing.as_table_like_mut().expect("child_mut checked"),
                    "overrides",
                )
            {
                stage_item
                    .as_table_like_mut()
                    .expect("a stage is a table")
                    .remove("tool_routing");
            }
            return Ok(());
        }
        let routing = ensure_child(stage_item, "tool_routing")?;
        let overrides = ensure_child(routing, "overrides")?;
        set_str(
            overrides.as_table_like_mut().expect("ensure_child checked"),
            tool,
            region,
        );
        Ok(())
    }

    /// The scope's `regions` item, mutably, when it exists.
    fn regions_item_mut(&mut self, scope: &RegionScope) -> Option<&mut Item> {
        let parent: &mut Item = match scope {
            RegionScope::Shared => self.doc_mut().as_item_mut(),
            RegionScope::Stage(name) => self.stage_item_mut(name)?,
        };
        let context = child_mut(parent, "context")?;
        child_mut(context, "regions")
    }

    /// The scope's `regions` item, created (with `context`) when missing.
    fn regions_item_ensure(&mut self, scope: &RegionScope) -> Result<&mut Item, EditError> {
        let parent: &mut Item = match scope {
            RegionScope::Shared => self.doc_mut().as_item_mut(),
            RegionScope::Stage(name) => self
                .stage_item_mut(name)
                .ok_or_else(|| EditError::NoSuchStage(name.clone()))?,
        };
        let context = ensure_child(parent, "context")?;
        ensure_child(context, "regions")
    }

    /// Rewrite (or, with `None`, clear) tool-routing references to a region.
    /// A shared region's rename touches every stage that inherits the shared
    /// layout; a stage region's touches that stage only.
    fn retarget_routing(&mut self, scope: &RegionScope, from: &str, to: Option<&str>) {
        let names = self.stage_names();
        for name in names {
            match scope {
                RegionScope::Stage(s) if s != &name => continue,
                RegionScope::Shared if self.regions_item(Some(&name)).is_some() => continue,
                _ => {}
            }
            let stage_item = self.stage_item_mut(&name).expect("listed stage exists");
            let Some(routing) = child_mut(stage_item, "tool_routing") else {
                continue;
            };
            let table = routing.as_table_like_mut().expect("child_mut checked");
            if get_str(table, "default_region") == Some(from) {
                match to {
                    Some(t) => set_str(table, "default_region", t),
                    None => {
                        table.remove("default_region");
                    }
                }
            }
            if let Some(overrides) = child_mut(routing, "overrides") {
                let o = overrides.as_table_like_mut().expect("child_mut checked");
                let hits: Vec<String> = o
                    .iter()
                    .filter(|(_, v)| v.as_str() == Some(from))
                    .map(|(k, _)| k.to_string())
                    .collect();
                for tool in hits {
                    match to {
                        Some(t) => set_str(o, &tool, t),
                        None => {
                            o.remove(&tool);
                        }
                    }
                }
                if to.is_none() && o.is_empty() {
                    routing
                        .as_table_like_mut()
                        .expect("child_mut checked")
                        .remove("overrides");
                }
            }
            if to.is_none()
                && routing
                    .as_table_like()
                    .expect("child_mut checked")
                    .is_empty()
            {
                stage_item
                    .as_table_like_mut()
                    .expect("a stage is a table")
                    .remove("tool_routing");
            }
        }
    }
}
