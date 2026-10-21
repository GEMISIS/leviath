//! A stage's `add`: regions it shows beside the graph's shared layout.
//!
//! ```toml
//! [[graph.stages]]
//! name = "plan"
//! add = { scratch = { kind = "pinned", budget = "10%" } }
//! ```
//!
//! `add` is a key of the file, not of the graph a run carries. Reading the
//! file folds it into the stage's own `layout`: the shared regions, with the
//! added ones after them, the shared total and eviction order. So every
//! reader of a graph (the runtime, `lev validate`, the API, a run file) sees
//! the layout the stage runs with, and checks it the way it checks any
//! layout a stage declares.
//!
//! A shared region keeps its place and its definition, and the stage's
//! `hide` still applies to it. Adding a name the shared layout already has
//! is refused unless the stage also hides it: `hide = ["notes"]` beside
//! `add.notes` is how a stage gives one shared region a definition of its
//! own, and that region is then shown, in the shared one's place.

use leviath_runtime::spec::graph::{
    ALWAYS_VISIBLE_REGIONS, RegionDef, RegionLayoutDef, RunGraph, StageDef,
};
use serde::Deserialize;
use toml::Spanned;
use toml::de::{DeString, DeTable, DeValue, Deserializer, ValueDeserializer};

use crate::file::BlueprintFile;

/// The key, in a stage's table.
const KEY: &str = "add";

/// One stage's `add`, by the stage's position in the graph.
struct Added {
    stage: usize,
    regions: Vec<RegionDef>,
}

/// Read `text` as a blueprint file, folding each stage's `add` into its
/// layout. Every error but the ones about `add` itself reads exactly as
/// `toml::from_str` would put it, with its line.
pub(crate) fn parse(text: &str) -> Result<BlueprintFile, String> {
    let mut root = DeTable::parse(text).map_err(|e| e.to_string())?;
    let added = take(root.get_mut(), text)?;
    let mut file = BlueprintFile::deserialize(Deserializer::from(root)).map_err(|mut e| {
        e.set_input(Some(text));
        e.to_string()
    })?;
    for Added { stage, regions } in added {
        fold(&mut file.graph, stage, regions)?;
    }
    Ok(file)
}

/// `shared` with `added` laid over it: a region of the same name takes the
/// added definition in its place, and the rest of `added` follows in order.
///
/// This is the one place a stage's inherited layout and its additions are
/// combined, whatever the region type: the loader merges region definitions,
/// the blueprint editor its views of them.
pub fn merge_regions<T>(shared: &[T], added: Vec<T>, name: impl Fn(&T) -> &str) -> Vec<T>
where
    T: Clone,
{
    let mut added: Vec<Option<T>> = added.into_iter().map(Some).collect();
    let mut out: Vec<T> = shared
        .iter()
        .map(|region| {
            added
                .iter_mut()
                .find(|a| a.as_ref().is_some_and(|a| name(a) == name(region)))
                .and_then(Option::take)
                .unwrap_or_else(|| region.clone())
        })
        .collect();
    out.extend(added.into_iter().flatten());
    out
}

/// Take every stage's `add` out of the parsed document, so the rest reads as
/// a graph, and read each into region definitions.
fn take(root: &mut DeTable<'_>, text: &str) -> Result<Vec<Added>, String> {
    let Some(DeValue::Table(graph)) = root.get_mut("graph").map(Spanned::get_mut) else {
        return Ok(Vec::new());
    };
    let Some(DeValue::Array(stages)) = graph.get_mut("stages").map(Spanned::get_mut) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (index, stage) in stages.iter_mut().enumerate() {
        let DeValue::Table(table) = stage.get_mut() else {
            continue;
        };
        if let Some(add) = table.remove(KEY) {
            let name = stage_name(table);
            out.push(Added {
                stage: index,
                regions: regions(&name, add, text)?,
            });
        }
    }
    Ok(out)
}

/// What a stage's table calls it, for a message.
fn stage_name(table: &DeTable<'_>) -> String {
    match table.get("name").map(Spanned::get_ref) {
        Some(DeValue::String(name)) => name.to_string(),
        _ => "(unnamed)".to_string(),
    }
}

/// Read one stage's `add` table: each key a region's name, each value the
/// rest of its definition.
fn regions(stage: &str, add: Spanned<DeValue<'_>>, text: &str) -> Result<Vec<RegionDef>, String> {
    let DeValue::Table(entries) = add.into_inner() else {
        return Err(format!(
            "stage \"{stage}\": `add` is a table of region definitions keyed by name, \
             such as add = {{ scratch = {{ kind = \"pinned\", budget = \"10%\" }} }}"
        ));
    };
    let mut out = Vec::new();
    for (key, value) in entries {
        let value_span = value.span();
        let value = match value.into_inner() {
            DeValue::Table(mut fields) => {
                if fields.contains_key("name") {
                    return Err(format!(
                        "stage \"{stage}\": add.{key} takes its name from its key; \
                         leave `name` out of it",
                        key = key.get_ref()
                    ));
                }
                let name = DeValue::String(DeString::Owned(key.get_ref().to_string()));
                fields.insert(
                    Spanned::new(key.span(), DeString::Borrowed("name")),
                    Spanned::new(key.span(), name),
                );
                DeValue::Table(fields)
            }
            other => other,
        };
        let region =
            RegionDef::deserialize(ValueDeserializer::from(Spanned::new(value_span, value)))
                .map_err(|mut e| {
                    e.set_input(Some(text));
                    e.to_string()
                })?;
        out.push(region);
    }
    Ok(out)
}

/// Give stage `index` the shared layout with `added` laid over it, after
/// checking that `add` says something it can mean.
fn fold(graph: &mut RunGraph, index: usize, added: Vec<RegionDef>) -> Result<(), String> {
    let shared = &graph.layout;
    let stage: &mut StageDef = &mut graph.stages[index];
    if stage.layout.is_some() {
        return Err(format!(
            "stage \"{}\" has both `layout` and `add`: `layout` already declares the \
             stage's whole layout, so put the region in it, or drop `layout` to inherit \
             the shared one and add to it",
            stage.name
        ));
    }
    for region in &added {
        let name = region.name.as_str();
        if ALWAYS_VISIBLE_REGIONS.contains(&name) {
            return Err(format!(
                "stage \"{}\" cannot add region \"{name}\": the runtime always shows it, \
                 whatever a stage declares",
                stage.name
            ));
        }
        let declared = shared.regions.iter().any(|r| r.name == region.name);
        if declared && !stage.hide.contains(&region.name) {
            return Err(format!(
                "stage \"{}\" adds region \"{name}\", which the shared layout already \
                 declares; to give this stage its own definition of it, list \"{name}\" in \
                 the stage's `hide` as well",
                stage.name
            ));
        }
    }
    stage
        .hide
        .retain(|hidden| !added.iter().any(|r| r.name == *hidden));
    stage.layout = Some(RegionLayoutDef {
        regions: merge_regions(&shared.regions, added, |r| r.name.as_str()),
        total_budget_tokens: shared.total_budget_tokens,
        eviction_order: shared.eviction_order.clone(),
    });
    Ok(())
}
