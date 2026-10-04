//! Mutators for context regions (`[graph.layout] regions` and a stage's own
//! `layout`), the inputs that fill them, and a stage's routing into them.
//!
//! Both layouts have the same shape, so one set of internals serves both;
//! [`RegionScope`] says which.

use toml_edit::{Array, InlineTable, Item, TableLike, Value};

use super::doc::{ManifestDoc, bound_regions};
use super::tables::{
    ensure_sub, get_str, inline_item, list_tables_mut, named_mut, push_table,
    remove_and_report_empty, retain_tables, set_bool, set_or_remove_int, set_or_remove_str,
    set_str, set_strings, set_value, sub, sub_mut,
};
use super::{EditError, require_name};

/// Whose regions: the graph's shared layout, or one stage's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RegionScope {
    /// `[graph.layout]`.
    Shared,
    /// The stage's own `layout`.
    Stage(String),
}

impl RegionScope {
    /// The stage name, for [`ManifestDoc::regions`] and friends.
    pub(crate) fn stage(&self) -> Option<&str> {
        match self {
            RegionScope::Shared => None,
            RegionScope::Stage(s) => Some(s),
        }
    }
}

/// The per-region settings the editor writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegionField {
    /// The kind's name (never deleted: a region always has one).
    Kind,
    /// The budget's percentage, 0 to 100.
    BudgetPercent,
    /// The budget's `max`, at least 1.
    MaxTokens,
    /// The budget's `min`, at least 1.
    MinTokens,
    /// `required = true`; off deletes the key.
    Required,
    /// `required_message`.
    RequiredMessage,
    /// The input that fills the region. A region with a `seed` of its own
    /// is never touched.
    Seed,
    /// A sliding window's `max_items`, at least 1.
    MaxItems,
    /// A sliding window's eviction: `per_item`, `bulk` or `compact`.
    Strategy,
    /// The count a `bulk` or `compact` eviction carries, at least 1.
    Overflow,
    /// `description`.
    Description,
    /// `accepts`, typed as a list: commas or spaces between patterns.
    Accepts,
}

/// A value for a [`RegionField`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RegionValue {
    /// For the text settings; empty deletes.
    Text(String),
    /// For the numeric settings; `None` deletes.
    Number(Option<u64>),
    /// For `required`.
    Flag(bool),
}

/// The count a bulk or compact eviction starts with.
const DEFAULT_EVICTION: i64 = 10;

/// What a new sliding window keeps.
const DEFAULT_WINDOW: i64 = 10;

impl ManifestDoc {
    /// Add a region to a layout with starter values (`pinned`, `5%`),
    /// creating the layout when the scope has none.
    ///
    /// No ceiling: a starter `max` is the thing that quietly undoes the
    /// percentage on every model with a window worth having, and a new region
    /// has no reason to want one. The author can add a floor or a ceiling
    /// deliberately.
    pub(crate) fn add_region(&mut self, scope: &RegionScope, name: &str) -> Result<(), EditError> {
        require_name(name)?;
        if self.region(scope.stage(), name).is_some() {
            return Err(EditError::Taken(name.to_string()));
        }
        let layout = self.layout_ensure(scope)?;
        if !layout.contains_key("regions") {
            layout.insert("regions", Item::Value(Value::Array(Array::new())));
        }
        let regions = layout
            .get_mut("regions")
            .expect("present or inserted just above");
        let mut region = InlineTable::new();
        region.insert("name", Value::from(name));
        region.insert("kind", Value::from("pinned"));
        region.insert("budget", Value::from("5%"));
        push_table(regions, region)
    }

    /// Rename a region, rewriting the tool routing that named it (every
    /// stage's for a shared region, except stages with their own layout,
    /// whose routing names their own regions; that stage's for its own) and,
    /// once no layout has a region of the old name, the inputs bound to it.
    pub(crate) fn rename_region(
        &mut self,
        scope: &RegionScope,
        from: &str,
        to: &str,
    ) -> Result<(), EditError> {
        if from == to {
            return Ok(());
        }
        require_name(to)?;
        if self.region(scope.stage(), from).is_none() {
            return Err(EditError::NoSuchRegion(from.to_string()));
        }
        if self.region(scope.stage(), to).is_some() {
            return Err(EditError::Taken(to.to_string()));
        }
        let region = self
            .region_table_mut(scope, from)
            .expect("checked just above");
        set_str(region, "name", to);
        self.retarget_routing(scope, from, Some(to));
        if !self.region_anywhere(from) {
            self.each_binding(&mut |bind| {
                if bind.get("region").and_then(Value::as_str) == Some(from) {
                    bind.insert("region", Value::from(to));
                }
            });
        }
        Ok(())
    }

    /// Delete a region, stop routing tool results into it wherever the
    /// scope's routing did, and, once no layout has a region of that name,
    /// unbind the inputs that filled it.
    pub(crate) fn delete_region(
        &mut self,
        scope: &RegionScope,
        name: &str,
    ) -> Result<(), EditError> {
        if self.region(scope.stage(), name).is_none() {
            return Err(EditError::NoSuchRegion(name.to_string()));
        }
        let regions = self
            .layout_mut(scope)
            .and_then(|l| l.get_mut("regions"))
            .expect("the region was found in this list");
        retain_tables(regions, &|r| get_str(r, "name") != Some(name));
        self.retarget_routing(scope, name, None);
        if !self.region_anywhere(name) {
            self.unbind(name);
        }
        Ok(())
    }

    /// Write one setting of a region.
    pub(crate) fn set_region_field(
        &mut self,
        scope: &RegionScope,
        name: &str,
        field: RegionField,
        value: RegionValue,
    ) -> Result<(), EditError> {
        if let (RegionField::Seed, RegionValue::Text(input)) = (field, &value) {
            return self.set_region_seed(scope, name, input);
        }
        let table = self.region_table_mut(scope, name)?;
        match (field, value) {
            (RegionField::Kind, RegionValue::Text(kind)) => set_kind(table, &kind),
            (RegionField::BudgetPercent, RegionValue::Number(Some(pct))) => {
                let text = format!("{}%", pct.min(100));
                match sub_mut(table, "budget") {
                    Some(budget) => set_str(budget, "percent", &text),
                    None => set_str(table, "budget", &text),
                }
            }
            (RegionField::BudgetPercent, RegionValue::Number(None)) => {
                table.remove("budget");
            }
            (RegionField::MaxTokens, RegionValue::Number(n)) => {
                set_clamp(table, "max", n.map(|n| n.max(1)))?
            }
            (RegionField::MinTokens, RegionValue::Number(n)) => {
                set_clamp(table, "min", n.map(|n| n.max(1)))?
            }
            (RegionField::MaxItems, RegionValue::Number(n)) => {
                let kind = kind_table(table);
                set_or_remove_int(kind, "max_items", n.map(|n| n.max(1)));
                tidy_kind(table);
            }
            (RegionField::Strategy, RegionValue::Text(t)) => set_strategy(table, &t)?,
            (RegionField::Overflow, RegionValue::Number(n)) => set_overflow(table, n)?,
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
            (RegionField::Description, RegionValue::Text(t)) => {
                set_or_remove_str(table, "description", &t);
            }
            (RegionField::Accepts, RegionValue::Text(t)) => {
                let list = super::mime::split_list(&t);
                if list.is_empty() {
                    table.remove("accepts");
                } else {
                    set_strings(table, "accepts", &list);
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

    /// Fill a region from the input called `input`: bind it there, declaring
    /// a text input of that name when there is none, and unbind every other
    /// input from the region (dropping an input left with nowhere to go).
    /// Empty only unbinds. A region with a `seed` of its own is left alone.
    fn set_region_seed(
        &mut self,
        scope: &RegionScope,
        region: &str,
        input: &str,
    ) -> Result<(), EditError> {
        if self.region_table_mut(scope, region)?.contains_key("seed") {
            return Ok(());
        }
        if !input.is_empty() {
            leviath_runtime::spec::names::InputName::new(input)
                .map_err(|_| EditError::BadName(input.to_string()))?;
            let binds = self
                .graph_list("inputs")
                .map(super::tables::list_tables)
                .unwrap_or_default()
                .into_iter()
                .find(|t| get_str(*t, "name") == Some(input))
                .and_then(|t| t.get("binds"));
            if binds.is_some_and(|b| !b.is_array()) {
                return Err(EditError::NotATable("binds".to_string()));
            }
        }
        let others: Vec<String> = self
            .region_bindings()
            .into_iter()
            .filter(|(name, regions)| name != input && regions.iter().any(|r| r == region))
            .map(|(name, _)| name)
            .collect();
        for other in &others {
            self.unbind_input(other, region);
        }
        if input.is_empty() {
            return Ok(());
        }
        let inputs = self.graph_list_mut("inputs")?;
        let mut bind = InlineTable::new();
        bind.insert("region", Value::from(region));
        match named_mut(inputs, input) {
            Some(existing) => {
                if bound_regions(&*existing).iter().any(|r| r == region) {
                    return Ok(());
                }
                if !existing.contains_key("binds") {
                    existing.insert("binds", Item::Value(Value::Array(Array::new())));
                }
                existing
                    .get_mut("binds")
                    .and_then(Item::as_array_mut)
                    .expect("checked to be a list, or made one just above")
                    .push(Value::InlineTable(bind));
            }
            None => {
                let mut entry = InlineTable::new();
                entry.insert("name", Value::from(input));
                entry.insert("type", Value::from("text"));
                let mut binds = Array::new();
                binds.push(Value::InlineTable(bind));
                entry.insert("binds", Value::Array(binds));
                push_table(inputs, entry).expect("graph_list_mut hands back a list");
            }
        }
        Ok(())
    }

    /// Give a stage its own layout: a copy of the shared layout as it is
    /// now, so it starts from what it inherited. A stage that already has
    /// one is left alone.
    pub(crate) fn create_stage_override(&mut self, stage: &str) -> Result<(), EditError> {
        self.require_stage(stage)?;
        if self.layout_table(Some(stage)).is_some() {
            return Ok(());
        }
        let shared: Option<Item> = self.graph().get("layout").cloned();
        let layout = match shared.and_then(|item| item.into_value().ok()) {
            Some(Value::InlineTable(mut copy)) => {
                copy.decor_mut().clear();
                copy
            }
            _ => starter_layout(),
        };
        let table = self.stage_table_mut(stage).expect("checked just above");
        table.insert("layout", inline_item(layout));
        Ok(())
    }

    /// Drop a stage's own layout, so it uses the shared regions again.
    pub(crate) fn remove_stage_override(&mut self, stage: &str) -> Result<(), EditError> {
        let table = self.stage_table_mut(stage)?;
        table.remove("layout");
        Ok(())
    }

    /// Set a stage's default region for tool results; empty removes it (and
    /// the `tool_routing` table when nothing else keeps it).
    pub(crate) fn set_tool_routing_default(
        &mut self,
        stage: &str,
        region: &str,
    ) -> Result<(), EditError> {
        let table = self.stage_table_mut(stage)?;
        if region.is_empty() {
            if let Some(routing) = sub_mut(table, "tool_routing")
                && remove_and_report_empty(routing, "default_region")
            {
                table.remove("tool_routing");
            }
            return Ok(());
        }
        let routing = ensure_sub(table, "tool_routing")?;
        set_str(routing, "default_region", region);
        Ok(())
    }

    /// Route one tool's results to a region; an empty region stops routing
    /// it, tidying emptied `tool_regions`/`tool_routing` tables away.
    pub(crate) fn set_tool_routing_override(
        &mut self,
        stage: &str,
        tool: &str,
        region: &str,
    ) -> Result<(), EditError> {
        let table = self.stage_table_mut(stage)?;
        if region.is_empty() {
            let Some(routing) = sub_mut(table, "tool_routing") else {
                return Ok(());
            };
            if let Some(overrides) = sub_mut(routing, "tool_regions")
                && remove_and_report_empty(overrides, tool)
                && remove_and_report_empty(routing, "tool_regions")
            {
                table.remove("tool_routing");
            }
            return Ok(());
        }
        let routing = ensure_sub(table, "tool_routing")?;
        let overrides = ensure_sub(routing, "tool_regions")?;
        set_str(overrides, tool, region);
        Ok(())
    }

    /// Rewrite a stage's `output_routing` from `entries` (mime pattern to
    /// region). An empty list removes the table. The whole table is rebuilt
    /// rather than diffed, since the editor edits it as one field.
    pub(crate) fn set_output_routing(
        &mut self,
        stage: &str,
        entries: &[(String, String)],
    ) -> Result<(), EditError> {
        let table = self.stage_table_mut(stage)?;
        if entries.is_empty() {
            table.remove("output_routing");
            return Ok(());
        }
        let routing = ensure_sub(table, "output_routing")?;
        let stale: Vec<String> = routing.iter().map(|(k, _)| k.to_string()).collect();
        for key in stale {
            routing.remove(&key);
        }
        for (pattern, region) in entries {
            set_str(routing, pattern, region);
        }
        Ok(())
    }

    /// Set a stage's `reset`, the regions emptied when it starts; an empty
    /// list deletes the key.
    pub(crate) fn set_context_reset(
        &mut self,
        stage: &str,
        regions: &[String],
    ) -> Result<(), EditError> {
        let table = self.stage_table_mut(stage)?;
        super::stages::set_or_remove_list(table, "reset", regions);
        Ok(())
    }

    /// The scope's layout table, mutably, when it exists.
    fn layout_mut(&mut self, scope: &RegionScope) -> Option<&mut dyn TableLike> {
        let parent: &mut dyn TableLike = match scope {
            RegionScope::Shared => self.graph_mut(),
            RegionScope::Stage(name) => self.stage_table_mut(name).ok()?,
        };
        sub_mut(parent, "layout")
    }

    /// The scope's layout table, created (with no regions and no fixed
    /// budget) when missing.
    fn layout_ensure(&mut self, scope: &RegionScope) -> Result<&mut dyn TableLike, EditError> {
        let parent: &mut dyn TableLike = match scope {
            RegionScope::Shared => self.graph_mut(),
            RegionScope::Stage(name) => self.stage_table_mut(name)?,
        };
        if !parent.contains_key("layout") {
            parent.insert("layout", inline_item(starter_layout()));
        }
        sub_mut(parent, "layout").ok_or_else(|| EditError::NotATable("layout".to_string()))
    }

    /// One region's table, mutably, or [`EditError::NoSuchRegion`].
    fn region_table_mut(
        &mut self,
        scope: &RegionScope,
        name: &str,
    ) -> Result<&mut dyn TableLike, EditError> {
        self.layout_mut(scope)
            .and_then(|l| l.get_mut("regions"))
            .and_then(|r| named_mut(r, name))
            .ok_or_else(|| EditError::NoSuchRegion(name.to_string()))
    }

    /// Whether any layout, the graph's or a stage's, has a region `name`.
    fn region_anywhere(&self, name: &str) -> bool {
        self.region(None, name).is_some()
            || self
                .stage_names()
                .iter()
                .any(|s| self.region(Some(s), name).is_some())
    }

    /// Drop every input's binding to `region`.
    fn unbind(&mut self, region: &str) {
        let names: Vec<String> = self
            .region_bindings()
            .into_iter()
            .filter(|(_, regions)| regions.iter().any(|r| r == region))
            .map(|(name, _)| name)
            .collect();
        for name in names {
            self.unbind_input(&name, region);
        }
    }

    /// Drop `input`'s binding to `region`, and the input itself when that
    /// leaves it binding nothing at all.
    fn unbind_input(&mut self, input: &str, region: &str) {
        let inputs = self
            .graph_mut()
            .get_mut("inputs")
            .expect("the input was read from this list");
        let binds = list_tables_mut(inputs)
            .into_iter()
            .filter(|entry| get_str(&**entry, "name") == Some(input))
            .filter_map(|entry| entry.get_mut("binds").and_then(Item::as_array_mut));
        for binds in binds {
            binds.retain(|b| {
                b.as_inline_table()
                    .and_then(|t| t.get("region"))
                    .and_then(Value::as_str)
                    != Some(region)
            });
        }
        retain_tables(inputs, &|entry| {
            get_str(entry, "name") != Some(input)
                || entry
                    .get("binds")
                    .and_then(Item::as_array)
                    .is_some_and(|b| !b.is_empty())
        });
    }

    /// Rewrite (or, with `None`, clear) tool-routing references to a region.
    /// A shared region's rename touches every stage that uses the shared
    /// layout; a stage region's touches that stage only.
    fn retarget_routing(&mut self, scope: &RegionScope, from: &str, to: Option<&str>) {
        for name in self.stage_names() {
            match scope {
                RegionScope::Stage(s) if s != &name => continue,
                RegionScope::Shared if self.layout_table(Some(&name)).is_some() => continue,
                _ => {}
            }
            let stage = self.stage_table_mut(&name).expect("listed stage exists");
            let Some(routing) = sub_mut(stage, "tool_routing") else {
                continue;
            };
            if get_str(&*routing, "default_region") == Some(from) {
                match to {
                    Some(t) => set_str(routing, "default_region", t),
                    None => {
                        routing.remove("default_region");
                    }
                }
            }
            if let Some(overrides) = sub_mut(routing, "tool_regions") {
                let hits: Vec<String> = overrides
                    .iter()
                    .filter(|(_, v)| v.as_str() == Some(from))
                    .map(|(k, _)| k.to_string())
                    .collect();
                for tool in hits {
                    match to {
                        Some(t) => set_str(overrides, &tool, t),
                        None => {
                            overrides.remove(&tool);
                        }
                    }
                }
                if to.is_none() && overrides.is_empty() {
                    routing.remove("tool_regions");
                }
            }
            if to.is_none() && routing.is_empty() {
                stage.remove("tool_routing");
            }
        }
    }
}

/// A layout with no regions and no fixed budget: every region sizes itself
/// as a share of the model's window.
fn starter_layout() -> InlineTable {
    let mut layout = InlineTable::new();
    layout.insert("total_budget_tokens", Value::from(0));
    layout.insert("regions", Value::Array(Array::new()));
    layout
}

/// Set a region's kind by name. A sliding window needs a size, so it is
/// written as a table with ten items; every other kind is written by name,
/// and a table already of that kind is kept.
fn set_kind(region: &mut dyn TableLike, kind: &str) {
    if kind.is_empty() {
        return;
    }
    let current = sub(&*region, "kind");
    if current.and_then(|k| get_str(k, "kind")) == Some(kind) {
        return;
    }
    if kind != "sliding_window" {
        set_str(region, "kind", kind);
        return;
    }
    let mut table = InlineTable::new();
    table.insert("kind", Value::from(kind));
    table.insert("max_items", Value::from(DEFAULT_WINDOW));
    set_value(region, "kind", Value::InlineTable(table));
}

/// A region's `kind` as a table, turned into one when it is a bare name.
fn kind_table(region: &mut dyn TableLike) -> &mut dyn TableLike {
    if let Some(name) = get_str(&*region, "kind").map(str::to_string) {
        let mut table = InlineTable::new();
        table.insert("kind", Value::from(name));
        set_value(region, "kind", Value::InlineTable(table));
    }
    ensure_sub(region, "kind").expect("a kind name became a table just above")
}

/// A kind table left holding only its name goes back to the bare name.
fn tidy_kind(region: &mut dyn TableLike) {
    let only_name = sub(&*region, "kind")
        .filter(|k| k.len() == 1)
        .and_then(|k| get_str(k, "kind"))
        .map(str::to_string);
    if let Some(name) = only_name {
        set_str(region, "kind", &name);
    }
}

/// Set a sliding window's eviction. `per_item` (or nothing) is the default
/// and is written as absent; `bulk` and `compact` carry a count, kept when
/// switching between them, ten to start.
fn set_strategy(region: &mut dyn TableLike, strategy: &str) -> Result<(), EditError> {
    let kind = kind_table(region);
    let count = sub(&*kind, "eviction")
        .and_then(|e| e.iter().next().and_then(|(_, v)| v.as_integer()))
        .unwrap_or(DEFAULT_EVICTION);
    match strategy {
        "" | "per_item" => {
            kind.remove("eviction");
        }
        "bulk" | "compact" => {
            let mut eviction = InlineTable::new();
            eviction.insert(strategy, Value::from(count));
            set_value(kind, "eviction", Value::InlineTable(eviction));
        }
        other => {
            tidy_kind(region);
            return Err(EditError::OutOfRange(format!(
                "\"{other}\" is not an eviction; pick per_item, bulk or compact"
            )));
        }
    }
    tidy_kind(region);
    Ok(())
}

/// Set the count of a bulk or compact eviction; `None` puts it back to ten.
fn set_overflow(region: &mut dyn TableLike, count: Option<u64>) -> Result<(), EditError> {
    let eviction = sub_mut(region, "kind").and_then(|k| sub_mut(k, "eviction"));
    let Some(eviction) = eviction else {
        return Err(EditError::OutOfRange(
            "only a bulk or compact eviction has a count; pick one first".to_string(),
        ));
    };
    let strategy = eviction
        .iter()
        .next()
        .map(|(k, _)| k.to_string())
        .unwrap_or_default();
    let count = count.map(|n| n.max(1)).unwrap_or(DEFAULT_EVICTION as u64);
    set_or_remove_int(eviction, &strategy, Some(count));
    Ok(())
}

/// Set (or, with `None`, remove) the `min` or `max` of a percentage budget,
/// writing the budget as a table while it has either and as the bare
/// percentage once it has neither.
fn set_clamp(region: &mut dyn TableLike, key: &str, value: Option<u64>) -> Result<(), EditError> {
    let percent = match region.get("budget") {
        Some(b) => b.as_str().map(str::to_string).or_else(|| {
            b.as_table_like()
                .and_then(|t| get_str(t, "percent"))
                .map(str::to_string)
        }),
        None => None,
    };
    let Some(percent) = percent else {
        return match value {
            None => Ok(()),
            Some(_) => Err(EditError::OutOfRange(
                "a floor or a ceiling goes on a percentage budget; set the share first".to_string(),
            )),
        };
    };
    if region.get("budget").and_then(Item::as_str).is_some() {
        let mut table = InlineTable::new();
        table.insert("percent", Value::from(percent.as_str()));
        set_value(region, "budget", Value::InlineTable(table));
    }
    let budget = sub_mut(region, "budget").expect("a table now");
    set_or_remove_int(budget, key, value);
    if budget.len() == 1 {
        set_str(region, "budget", &percent);
    }
    Ok(())
}
