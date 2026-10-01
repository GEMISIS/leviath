//! Reading and writing table-shaped TOML without caring which shape it is
//! in.
//!
//! An `agent.toml` writes the same thing several ways: a stage as a
//! `[[graph.stages]]` table or as `{ name = "plan", ... }` inside a
//! `stages = [...]` array, a stage's tool routing as a `tool_routing = {...}`
//! inline table or a `[graph.stages.tool_routing]` header. `toml_edit` keeps
//! these as different types (`Table` and `InlineTable`, `ArrayOfTables` and
//! `Array`) behind one trait, `TableLike`. Everything here works on the trait
//! and on both list shapes, so an edit never turns what an author wrote into
//! the other shape. A table the editor creates is always inline: it lands on
//! the line of the key that holds it, wherever the parent is written.

use toml_edit::{Array, InlineTable, Item, Table, TableLike, Value};

use super::EditError;

/// The table under `key`, if there is one.
pub(super) fn sub<'a>(table: &'a dyn TableLike, key: &str) -> Option<&'a dyn TableLike> {
    table.get(key).and_then(Item::as_table_like)
}

/// The table under `key`, mutably, if there is one.
pub(super) fn sub_mut<'a>(
    table: &'a mut dyn TableLike,
    key: &str,
) -> Option<&'a mut dyn TableLike> {
    table.get_mut(key).and_then(Item::as_table_like_mut)
}

/// The table under `key`, created empty and inline when missing. Refuses
/// when the key holds something that is not a table.
pub(super) fn ensure_sub<'a>(
    table: &'a mut dyn TableLike,
    key: &str,
) -> Result<&'a mut dyn TableLike, EditError> {
    if !table.contains_key(key) {
        table.insert(key, inline_item(InlineTable::new()));
    }
    table
        .get_mut(key)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| EditError::NotATable(key.to_string()))
}

/// An inline table as an item.
pub(super) fn inline_item(table: InlineTable) -> Item {
    Item::Value(Value::InlineTable(table))
}

/// A string value under `key`, when it is one.
pub(super) fn get_str<'a>(table: &'a dyn TableLike, key: &str) -> Option<&'a str> {
    table.get(key)?.as_str()
}

/// An integer under `key`, when it is one.
pub(super) fn get_int(table: &dyn TableLike, key: &str) -> Option<i64> {
    table.get(key)?.as_integer()
}

/// A non-negative integer under `key`, when it is one.
pub(super) fn get_count(table: &dyn TableLike, key: &str) -> Option<u64> {
    get_int(table, key).and_then(|n| u64::try_from(n).ok())
}

/// A boolean under `key`, when it is one.
pub(super) fn get_bool(table: &dyn TableLike, key: &str) -> Option<bool> {
    table.get(key)?.as_bool()
}

/// The strings of an array under `key`; anything else in it is skipped.
pub(super) fn get_strings(table: &dyn TableLike, key: &str) -> Vec<String> {
    table
        .get(key)
        .and_then(Item::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Write a string, or remove the key when it is empty: an absent key and an
/// empty string mean the same thing to the runtime, and absent keeps the
/// file tidy.
pub(super) fn set_or_remove_str(table: &mut dyn TableLike, key: &str, value: &str) {
    if value.is_empty() {
        table.remove(key);
    } else {
        set_str(table, key, value);
    }
}

/// Write a string, keeping the key's place when it already exists.
pub(super) fn set_str(table: &mut dyn TableLike, key: &str, value: &str) {
    set_value(table, key, Value::from(value));
}

/// Write an integer, keeping the key's place when it already exists.
pub(super) fn set_int(table: &mut dyn TableLike, key: &str, value: i64) {
    set_value(table, key, Value::from(value));
}

/// Write a boolean, keeping the key's place when it already exists.
pub(super) fn set_bool(table: &mut dyn TableLike, key: &str, value: bool) {
    set_value(table, key, Value::from(value));
}

/// Write an array of strings, keeping the key's place when it exists.
pub(super) fn set_strings(table: &mut dyn TableLike, key: &str, values: &[String]) {
    set_value(table, key, Value::Array(string_array(values)));
}

/// An array of strings.
pub(super) fn string_array(values: &[String]) -> Array {
    let mut array = Array::new();
    for v in values {
        array.push(v.as_str());
    }
    array
}

/// Write an integer, or remove the key for `None`.
pub(super) fn set_or_remove_int(table: &mut dyn TableLike, key: &str, value: Option<u64>) {
    match value {
        Some(n) => set_int(table, key, clamp_i64(n)),
        None => {
            table.remove(key);
        }
    }
}

/// TOML integers are signed 64-bit; anything bigger is written as the top.
pub(super) fn clamp_i64(n: u64) -> i64 {
    n.min(i64::MAX as u64) as i64
}

/// Put `value` under `key` in place: an existing value keeps its position
/// and its surrounding whitespace, a new one goes at the end. Whatever held
/// the key before (a headed table included) is replaced.
pub(super) fn set_value(table: &mut dyn TableLike, key: &str, mut value: Value) {
    if let Some(existing) = table.get_mut(key).and_then(Item::as_value_mut) {
        // Keep the author's spacing around the old value.
        *value.decor_mut() = existing.decor().clone();
        *existing = value;
    } else {
        table.insert(key, Item::Value(value));
    }
}

/// Remove `key` and, when that leaves the table empty, say so.
pub(super) fn remove_and_report_empty(table: &mut dyn TableLike, key: &str) -> bool {
    table.remove(key);
    table.is_empty()
}

// ── Lists of tables ─────────────────────────────────────────────────────────

/// The tables of a list, whichever shape it has: `[[...]]` tables, or an
/// array of inline tables (anything else in the array is skipped). Nothing
/// for a value that is not a list.
pub(super) fn list_tables(list: &Item) -> Vec<&dyn TableLike> {
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

/// [`list_tables`], mutably.
pub(super) fn list_tables_mut(list: &mut Item) -> Vec<&mut dyn TableLike> {
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

/// The position, among a list's tables, of the one whose `name` is `name`.
pub(super) fn index_named(list: &Item, name: &str) -> Option<usize> {
    list_tables(list)
        .iter()
        .position(|t| get_str(*t, "name") == Some(name))
}

/// The table of a list whose `name` is `name`, mutably.
pub(super) fn named_mut<'a>(list: &'a mut Item, name: &str) -> Option<&'a mut dyn TableLike> {
    list_tables_mut(list)
        .into_iter()
        .find(|t| get_str(&**t, "name") == Some(name))
}

/// Insert `entry` as the `index`th table of a list (or last, past the end),
/// in the list's shape. An inline entry copies the spacing of its
/// neighbour, so a list written one entry per line stays that way.
pub(super) fn insert_table(
    list: &mut Item,
    index: usize,
    entry: InlineTable,
) -> Result<(), EditError> {
    match list {
        Item::ArrayOfTables(tables) => {
            let mut all: Vec<Table> = tables.iter().cloned().collect();
            let at = index.min(all.len());
            all.insert(at, entry.into_table());
            tables.clear();
            for t in all {
                tables.push(t);
            }
            Ok(())
        }
        Item::Value(Value::Array(array)) => {
            let raw = raw_index(array, index);
            let value = Value::InlineTable(entry);
            // The first entry is written flush against the bracket, so only
            // a later one says how entries are spaced.
            match array.len() > 1 {
                true => {
                    let spacing = entry_spacing(array.get(array.len() - 1).expect("len > 1"));
                    array.insert_formatted(raw, value.decorated(spacing, ""));
                }
                false => array.insert(raw, value),
            }
            Ok(())
        }
        _ => Err(EditError::NotATable("a list".to_string())),
    }
}

/// Append `entry` to a list, in the list's shape.
pub(super) fn push_table(list: &mut Item, entry: InlineTable) -> Result<(), EditError> {
    insert_table(list, usize::MAX, entry)
}

/// Drop the `index`th table of a list; `false` when there is none. An
/// inline list is counted by its tables, the way it is read, so a stray
/// value in it neither shifts the count nor gets removed in a table's place.
pub(super) fn remove_table(list: &mut Item, index: usize) -> bool {
    match list {
        Item::ArrayOfTables(tables) if index < tables.len() => {
            tables.remove(index);
            true
        }
        Item::Value(Value::Array(array)) => {
            let raw = table_positions(array).nth(index);
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

/// Keep only the tables of a list `keep` says yes to.
pub(super) fn retain_tables(list: &mut Item, keep: &dyn Fn(&dyn TableLike) -> bool) {
    let drop: Vec<usize> = list_tables(list)
        .iter()
        .enumerate()
        .filter(|(_, t)| !keep(**t))
        .map(|(i, _)| i)
        .collect();
    for i in drop.into_iter().rev() {
        remove_table(list, i);
    }
}

/// The whitespace before an array entry, without any comment above it: a
/// new line and the indent when the entry starts a line, else what is there.
fn entry_spacing(entry: &Value) -> String {
    let prefix = entry
        .decor()
        .prefix()
        .and_then(|p| p.as_str())
        .unwrap_or(" ");
    match prefix.rsplit_once('\n') {
        Some((_, indent)) => format!("\n{indent}"),
        None => prefix.to_string(),
    }
}

/// Where in an array its tables sit.
fn table_positions(array: &Array) -> impl Iterator<Item = usize> + '_ {
    array
        .iter()
        .enumerate()
        .filter(|(_, v)| v.is_inline_table())
        .map(|(i, _)| i)
}

/// The array position the `index`th table goes at: before the table now
/// there, or at the end.
fn raw_index(array: &Array, index: usize) -> usize {
    table_positions(array).nth(index).unwrap_or(array.len())
}
