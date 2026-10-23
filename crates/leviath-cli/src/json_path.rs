//! Reading JSON into a type, and saying where it went wrong.
//!
//! Serde's own message names the missing or unknown key but not where in the
//! document it sits, which leaves a caller repairing a graph of dozens of
//! regions and stages with nothing to search for. These errors lead with the
//! path of the failure and, when the object there has a `name`, that name:
//! `layout.regions[12] (error_report): missing field `kind``.

use serde_json::Value;
use serde_path_to_error::{Path, Segment};

/// `value` as a `T`, or the reason it is not one, led by where it failed.
pub(crate) fn read<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, String> {
    let probe = value.clone();
    serde_path_to_error::deserialize(value)
        .map_err(|error| located(&probe, error.path(), &error.inner().to_string()))
}

/// `message` for a failure at `path` in `document`, led by that path (and the
/// nearest enclosing object's `name`). A failure at the root has no path to
/// lead with.
fn located(document: &Value, path: &Path, message: &str) -> String {
    if path.iter().next().is_none() {
        return message.to_string();
    }
    let at = path.to_string();
    match nearest_name(document, path) {
        Some(name) => format!("{at} ({name}): {message}"),
        None => format!("{at}: {message}"),
    }
}

/// The `name` of the deepest object along `path` that has one.
fn nearest_name(document: &Value, path: &Path) -> Option<String> {
    let mut name = None;
    let mut here = Some(document);
    for segment in path.iter() {
        let Some(current) = here else { break };
        if let Some(found) = current.get("name").and_then(Value::as_str) {
            name = Some(found.to_string());
        }
        here = match segment {
            Segment::Map { key } => current.get(key.as_str()),
            Segment::Seq { index } => current.get(*index),
            _ => None,
        };
    }
    if let Some(found) = here.and_then(|v| v.get("name")).and_then(Value::as_str) {
        name = Some(found.to_string());
    }
    name
}

#[cfg(test)]
#[path = "json_path_tests.rs"]
mod tests;
