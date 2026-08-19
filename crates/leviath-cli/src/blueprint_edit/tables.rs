//! Reading and writing table-shaped TOML without caring which of the two
//! shapes it is in.
//!
//! A manifest writes the same thing two ways: `[stages.plan.transitions.x]`
//! as a table with a header, or `transitions = { x = { hint = "..." } }`
//! inline. `toml_edit` keeps them as different types (`Table` and
//! `InlineTable`) behind one trait, `TableLike`. Everything here works on the
//! trait, and when it has to create a child it creates one of the parent's
//! shape, so an edit never turns an author's inline table into a headed one
//! or the other way round.

use toml_edit::{Array, InlineTable, Item, Key, Table, TableLike, Value};

use super::EditError;

/// The table an item is, if it is one.
pub(super) fn as_table(item: &Item) -> Option<&dyn TableLike> {
    item.as_table_like()
}

/// A child of `item` that is itself a table.
pub(super) fn child<'a>(item: &'a Item, key: &str) -> Option<&'a dyn TableLike> {
    item.as_table_like()
        .and_then(|t| t.get(key))
        .and_then(Item::as_table_like)
}

/// A child of `item` that is a table, mutably.
pub(super) fn child_mut<'a>(item: &'a mut Item, key: &str) -> Option<&'a mut Item> {
    item.as_table_like_mut()
        .and_then(|t| t.get_mut(key))
        .filter(|c| c.as_table_like().is_some())
}

/// The child table `key` of `item` (which must be a table), created (empty,
/// in the parent's shape) when missing. Refuses when the key holds
/// something that is not a table.
pub(super) fn ensure_child<'a>(item: &'a mut Item, key: &str) -> Result<&'a mut Item, EditError> {
    let inline = item.is_inline_table();
    let table = item.as_table_like_mut().expect("callers pass a table");
    if !table.contains_key(key) {
        table.insert(key, new_table(inline));
    }
    let child = table.get_mut(key).expect("inserted or present just above");
    if child.as_table_like().is_none() {
        return Err(EditError::NotATable(key.to_string()));
    }
    Ok(child)
}

/// An empty table of the shape a parent uses.
pub(super) fn new_table(inline: bool) -> Item {
    if inline {
        Item::Value(Value::InlineTable(InlineTable::new()))
    } else {
        let mut table = Table::new();
        table.set_implicit(false);
        Item::Table(table)
    }
}

/// A string value under `key`, when it is one.
pub(super) fn get_str<'a>(table: &'a dyn TableLike, key: &str) -> Option<&'a str> {
    table.get(key)?.as_str()
}

/// An integer under `key`, when it is one.
pub(super) fn get_int(table: &dyn TableLike, key: &str) -> Option<i64> {
    table.get(key)?.as_integer()
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
    replace_value(table, key, Value::from(value));
}

/// Write an integer, keeping the key's place when it already exists.
pub(super) fn set_int(table: &mut dyn TableLike, key: &str, value: i64) {
    replace_value(table, key, Value::from(value));
}

/// Write a boolean, keeping the key's place when it already exists.
pub(super) fn set_bool(table: &mut dyn TableLike, key: &str, value: bool) {
    replace_value(table, key, Value::from(value));
}

/// Write an array of strings, keeping the key's place when it exists.
pub(super) fn set_strings(table: &mut dyn TableLike, key: &str, values: &[String]) {
    let mut array = Array::new();
    for v in values {
        array.push(v.as_str());
    }
    replace_value(table, key, Value::Array(array));
}

/// Put `value` under `key` in place: an existing value keeps its position
/// and its surrounding whitespace, a new one goes at the end.
fn replace_value(table: &mut dyn TableLike, key: &str, mut value: Value) {
    if let Some(existing) = table.get_mut(key).and_then(Item::as_value_mut) {
        // Keep the author's spacing around the old value.
        *value.decor_mut() = existing.decor().clone();
        *existing = value;
    } else {
        table.insert(key, Item::Value(value));
    }
}

/// Rename a key in place: the entry keeps its position among its siblings
/// and the comments above it. `false` when `from` is not there.
pub(super) fn rename_key(table: &mut dyn TableLike, from: &str, to: &str) -> bool {
    if !table.contains_key(from) {
        return false;
    }
    let entries: Vec<(Key, Item)> = table
        .iter()
        .map(|(name, item)| {
            let key = table.key(name).expect("iterated key exists").clone();
            (key, item.clone())
        })
        .collect();
    table.clear();
    for (key, item) in entries {
        let key = if key.get() == from {
            Key::new(to).with_leaf_decor(key.leaf_decor().clone())
        } else {
            key
        };
        table.entry_format(&key).or_insert(item);
    }
    true
}

/// Remove `key` and, when that leaves the table empty, say so.
pub(super) fn remove_and_report_empty(table: &mut dyn TableLike, key: &str) -> bool {
    table.remove(key);
    table.is_empty()
}

/// The names of the child tables of `item`, in document order.
pub(super) fn table_keys(item: &Item) -> Vec<String> {
    item.as_table_like()
        .map(|t| {
            t.iter()
                .filter(|(_, v)| v.as_table_like().is_some())
                .map(|(k, _)| k.to_string())
                .collect()
        })
        .unwrap_or_default()
}
