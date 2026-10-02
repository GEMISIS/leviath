//! A run file's contents as TOML, for people to read.
//!
//! The binary frames are for the machine; these views are what `lev run show`
//! prints. TOML has no null and no integer past `i64::MAX`, so the values go
//! through JSON first: an absent optional field is left out (unless it is the
//! one field of a change that clears it), a `null` inside a list or a cleared
//! field reads as the string `"none"`, and an integer too large for TOML is
//! written as a string of its digits.

use serde::Serialize;
use serde_json::Value;

use crate::spec::run_spec::RunSpec;
use crate::state::{RunState, StateDelta};

/// The spec, under a `[spec]` table.
pub fn spec_toml(spec: &RunSpec) -> String {
    render("spec", spec)
}

/// A state, under a `[state]` table.
pub fn state_toml(state: &RunState) -> String {
    render("state", state)
}

/// Deltas, as a `[[delta]]` array of tables.
pub fn deltas_toml(deltas: &[StateDelta]) -> String {
    render("delta", deltas)
}

fn render<T: Serialize + ?Sized>(key: &str, value: &T) -> String {
    let value = serde_json::to_value(value).expect("run file types are plain data");
    let mut root = serde_json::Map::new();
    root.insert(key.to_string(), tomlable(value));
    toml::to_string_pretty(&Value::Object(root)).expect("tomlable values are valid TOML")
}

/// `value` with every shape TOML cannot hold replaced by one it can.
///
/// An object of one field, and that `null`, keeps it, read as `"none"`: that
/// is a change clearing a field (`{ "Pending": null }`), and leaving the field
/// out would leave an empty table that says nothing. A table of several
/// settings all unset is an empty table, the way an unset setting is absent
/// anywhere else.
fn tomlable(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let cleared = map.len() == 1 && map.values().all(Value::is_null);
            Value::Object(
                map.into_iter()
                    .filter(|(_, v)| cleared || !v.is_null())
                    .map(|(k, v)| (k, tomlable(v)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(tomlable).collect()),
        Value::Null => Value::String("none".to_string()),
        Value::Number(n) if n.is_u64() && n.as_i64().is_none() => Value::String(n.to_string()),
        other => other,
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
