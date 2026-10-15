//! An `agent.toml` whose fan-outs name one `worker`, rewritten in the form
//! this build reads: each fan-out names the stage it belongs to and lists
//! its `workers_available`.
//!
//! Builds before fan-out worker choices wrote
//! `mode = { fan_out = { worker = { stage = "review_worker" } } }`. The same
//! fan-out now reads
//! `mode = { fan_out = { stage = "split_review", workers_available = [{ name
//! = "review_worker", stage = "review_worker", use_for = "..." }] } }`: one
//! worker, named after what it runs, for what its stage's description says,
//! or for every item when it says nothing. The rest of the file, comments
//! and layout included, is left as it was.

use leviath_runtime::spec::graph::{EVERY_ITEM, worker_name_from};
use toml_edit::{Array, DocumentMut, InlineTable, Item, TableLike, Value};

use super::tables::{get_str, list_tables, list_tables_mut, sub, sub_mut};

/// One fan-out the rewrite changed: its stage, and the worker it now
/// declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rewritten {
    /// The fan-out's stage.
    pub stage: String,
    /// The name its one worker now goes by.
    pub worker: String,
}

/// Whether `text` is TOML with a fan-out that names one `worker`, the form
/// [`rewrite_fan_outs`] rewrites.
pub(crate) fn names_one_worker(text: &str) -> bool {
    let Ok(doc) = text.parse::<DocumentMut>() else {
        return false;
    };
    doc.get("graph")
        .and_then(Item::as_table_like)
        .and_then(|graph| graph.get("stages"))
        .map(list_tables)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|stage| sub(stage, "mode").and_then(|m| sub(m, "fan_out")))
        .any(|fan_out| fan_out.contains_key("worker"))
}

/// `text`, an `agent.toml`, with every fan-out that names one `worker`
/// rewritten to declare `workers_available`, and what changed. `Ok(None)`
/// when no fan-out names one; `Err` when the text is not TOML, or a
/// `worker` names nothing this build can read.
pub(crate) fn rewrite_fan_outs(text: &str) -> Result<Option<(String, Vec<Rewritten>)>, String> {
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("{e}"))?;
    let Some(stages) = doc
        .get_mut("graph")
        .and_then(Item::as_table_like_mut)
        .and_then(|graph| graph.get_mut("stages"))
    else {
        return Ok(None);
    };
    let described: Vec<(String, String)> = list_tables(stages)
        .into_iter()
        .filter_map(|s| {
            Some((
                get_str(s, "name")?.to_string(),
                get_str(s, "description")?.to_string(),
            ))
        })
        .collect();
    let mut changed = Vec::new();
    for stage in list_tables_mut(stages) {
        let name = get_str(&*stage, "name").unwrap_or_default().to_string();
        let Some(fan_out) = sub_mut(stage, "mode").and_then(|m| sub_mut(m, "fan_out")) else {
            continue;
        };
        let Some(worker) = sub(&*fan_out, "worker") else {
            continue;
        };
        let (key, value, named) = target_of(worker)
            .ok_or_else(|| format!("stage '{name}': its fan-out's `worker` names nothing"))?;
        let use_for = described
            .iter()
            .find(|(stage, _)| key == "stage" && *stage == value)
            .map_or(EVERY_ITEM.to_string(), |(_, description)| {
                description.clone()
            });
        let worker_name = worker_name_from(&named).to_string();
        let mut entry = InlineTable::new();
        entry.insert("name", Value::from(worker_name.as_str()));
        entry.insert(key, Value::from(value.as_str()));
        entry.insert("use_for", Value::from(use_for.as_str()));
        let mut workers = Array::new();
        workers.push(Value::InlineTable(entry));
        fan_out.remove("worker");
        if !fan_out.contains_key("stage") {
            fan_out.insert("stage", Item::Value(Value::from(name.as_str())));
        }
        fan_out.insert("workers_available", Item::Value(Value::Array(workers)));
        changed.push(Rewritten {
            stage: name,
            worker: worker_name,
        });
    }
    Ok((!changed.is_empty()).then(|| (doc.to_string(), changed)))
}

/// What an old `worker` table runs, as the key and value a
/// `workers_available` entry writes it with, and the text its name is made
/// from: `{ stage = "x" }`, `{ query = "x" }`, `{ blueprint_file = "/p" }`,
/// or `{ blueprint = { name = "x", digest = "d" } }`.
fn target_of(worker: &dyn TableLike) -> Option<(&'static str, String, String)> {
    if let Some(stage) = get_str(worker, "stage") {
        return Some(("stage", stage.to_string(), stage.to_string()));
    }
    if let Some(query) = get_str(worker, "query") {
        return Some(("query", query.to_string(), String::new()));
    }
    if let Some(path) = get_str(worker, "blueprint_file") {
        let last = path
            .trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default();
        return Some(("blueprint", path.to_string(), last.to_string()));
    }
    let blueprint = sub(worker, "blueprint")?;
    let name = get_str(blueprint, "name")?;
    let value = match get_str(blueprint, "digest") {
        Some(digest) => format!("{name}@{digest}"),
        None => name.to_string(),
    };
    Some(("blueprint", value, name.to_string()))
}

#[cfg(test)]
#[path = "fan_out_form_tests.rs"]
mod tests;
