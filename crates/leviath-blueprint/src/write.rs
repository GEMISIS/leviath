//! Writing a blueprint as TOML a person can read.
//!
//! Serde writes every field, so a straight dump of a graph is mostly
//! `hide = []` and `required = false`. The writer leaves out each key whose
//! absence reads back as the same file, and checks that by reading the file
//! back rather than by keeping its own list of defaults, so it cannot drift
//! from the types. Then it writes short tables inline, so a region's budget
//! is `budget = { tokens = 8000 }` rather than a section of its own.

use std::collections::BTreeSet;

use toml::Value;
use toml_edit::{DocumentMut, Item, Table};

use crate::file::BlueprintFile;

/// Words that are the default of some enum field in a graph. A key holding
/// one is worth trying to leave out; any other text is not.
const DEFAULT_WORDS: &[&str] = &[
    "always",
    "at_spawn",
    "auto_approve",
    "autonomous",
    "continue",
    "direct",
    "error",
    "evict",
    "free_text",
    "none",
    "once",
    "per_item",
    "reject",
    "rewritten",
];

/// The widest a table may be, written inline, and still be written that way.
const INLINE_WIDTH: usize = 72;

/// One step into a TOML value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Step {
    Key(String),
    Index(usize),
}

/// A key that might be left out: the table it sits in, and its name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Candidate {
    table: Vec<Step>,
    key: String,
}

pub(crate) fn render(file: &BlueprintFile) -> Result<String, String> {
    let full = Value::try_from(file).map_err(|e| format!("cannot be written as TOML: {e}"))?;
    let mut value = full.clone();
    minimize(&mut value, file);
    refill(&mut value, &full);
    let text = toml::to_string_pretty(&value).expect("a TOML value always writes");
    let mut doc: DocumentMut = text.parse().expect("TOML written by `toml` parses");
    tidy(
        doc.as_table_mut(),
        value.as_table().expect("a file is a table"),
        true,
    );
    Ok(doc.to_string())
}

/// Leave out every key whose absence reads back as `target`.
fn minimize(value: &mut Value, target: &BlueprintFile) {
    let mut kept = BTreeSet::new();
    loop {
        let mut found = Vec::new();
        candidates(value, &mut Vec::new(), &mut found);
        found.retain(|c| !kept.contains(c));
        if found.is_empty() {
            return;
        }
        settle(value, target, &found, &mut kept);
    }
}

/// Try leaving out all of `cands` at once; when that changes the file, split
/// them in half and try each half, down to single keys, which are kept.
fn settle(
    value: &mut Value,
    target: &BlueprintFile,
    cands: &[Candidate],
    kept: &mut BTreeSet<Candidate>,
) {
    let mut trial = value.clone();
    for c in cands {
        remove(&mut trial, c);
    }
    if reads_as(&trial, target) {
        *value = trial;
        return;
    }
    match cands {
        [one] => {
            kept.insert(one.clone());
        }
        _ => {
            let (a, b) = cands.split_at(cands.len() / 2);
            settle(value, target, a, kept);
            settle(value, target, b, kept);
        }
    }
}

/// Give a table that leaving out defaults emptied, but that the file still
/// holds, back its first key. `sandbox = {}` reads as the same file as
/// `sandbox = { kind = "none" }`, and only the second says what it means.
fn refill(value: &mut Value, full: &Value) {
    match (value, full) {
        (Value::Table(table), Value::Table(original)) => {
            if table.is_empty() {
                table.extend(original.iter().take(1).map(|(k, v)| (k.clone(), v.clone())));
                return;
            }
            for (key, child) in table.iter_mut() {
                refill(child, &original[key.as_str()]);
            }
        }
        (Value::Array(items), Value::Array(original)) => {
            for (child, was) in items.iter_mut().zip(original) {
                refill(child, was);
            }
        }
        _ => {}
    }
}

fn reads_as(value: &Value, target: &BlueprintFile) -> bool {
    value.clone().try_into::<BlueprintFile>().ok().as_ref() == Some(target)
}

fn candidates(value: &Value, at: &mut Vec<Step>, out: &mut Vec<Candidate>) {
    match value {
        Value::Table(t) => {
            for (key, child) in t {
                if looks_default(child) {
                    out.push(Candidate {
                        table: at.clone(),
                        key: key.clone(),
                    });
                }
                at.push(Step::Key(key.clone()));
                candidates(child, at, out);
                at.pop();
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                at.push(Step::Index(i));
                candidates(child, at, out);
                at.pop();
            }
        }
        _ => {}
    }
}

fn looks_default(value: &Value) -> bool {
    match value {
        Value::Boolean(_) => true,
        Value::Array(items) => items.is_empty(),
        Value::Table(t) => t.is_empty(),
        Value::String(s) => DEFAULT_WORDS.contains(&s.as_str()),
        _ => false,
    }
}

fn remove(value: &mut Value, c: &Candidate) {
    let mut node = value;
    for step in &c.table {
        node = match step {
            Step::Key(k) => &mut node[k.as_str()],
            Step::Index(i) => &mut node[*i],
        };
    }
    node.as_table_mut()
        .expect("a candidate sits in a table")
        .remove(&c.key);
}

/// Write every table below the top level inline when it is short, and put
/// each table's keys back in the order the types declare them (`order` is the
/// same table as a value, in that order).
fn tidy(table: &mut Table, order: &toml::Table, top: bool) {
    for (key, item) in table.iter_mut() {
        let same = &order[key.get()];
        if let Some(t) = item.as_table_mut() {
            tidy(t, same.as_table().expect("the same shape"), false);
        }
        if let Some(list) = item.as_array_of_tables_mut() {
            let originals = same.as_array().expect("the same shape");
            for (t, o) in list.iter_mut().zip(originals) {
                tidy(t, o.as_table().expect("the same shape"), false);
            }
        }
        if !top {
            shrink(item);
        }
    }
    let rank = |k: &toml_edit::Key| order.keys().position(|o| o == k.get());
    table.sort_values_by(|a, _, b, _| rank(a).cmp(&rank(b)));
    table.fmt();
}

/// Write a table, or a list of tables, inline when it is short. A list whose
/// every entry is short is written one entry per line.
fn shrink(item: &mut Item) {
    let value = item
        .clone()
        .into_value()
        .expect("a table's entries are tables or values");
    if one_line(&value) {
        *item = Item::Value(value);
        return;
    }
    if let toml_edit::Value::Array(mut list) = value
        && list.iter().all(one_line)
    {
        for entry in list.iter_mut() {
            entry.decor_mut().set_prefix("\n    ");
            entry.decor_mut().set_suffix("");
        }
        list.set_trailing("\n");
        list.set_trailing_comma(true);
        *item = Item::Value(toml_edit::Value::Array(list));
    }
}

fn one_line(value: &toml_edit::Value) -> bool {
    let text = value.to_string();
    text.len() <= INLINE_WIDTH && !text.contains('\n')
}
