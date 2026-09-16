//! Reasoning items, sealed under the provider that produced them.
//!
//! A Responses route hands back its chain of thought as opaque
//! `encrypted_content` items that only that vendor can read. History is
//! replayed to whichever provider runs the next turn or stage, so an item
//! from Meta handed to xAI is a hard 400, and one handed to a model that
//! cannot read it wastes the request. Each blob therefore names its provider:
//!
//! ```json
//! {"responses": {"provider": "meta", "items": ["<encrypted_content>", ...]}}
//! ```
//!
//! A blob that is not JSON is a single Codex item, which is the form a Codex
//! run stores. The Bedrock provider keeps `{"bedrock": [...]}` in the same
//! field, and neither reader takes the other's.

use serde_json::{Value, json};

/// The key a sealed blob's object sits under.
pub const KEY: &str = "responses";

/// Seal `items` for `provider`. `None` when there is nothing to keep, so an
/// empty turn stores no reasoning at all.
pub fn seal(provider: &str, items: &[String]) -> Option<String> {
    (!items.is_empty())
        .then(|| json!({ KEY: { "provider": provider, "items": items } }).to_string())
}

/// The items `blob` holds for `provider`: every item when the provider
/// sealed it, none when another provider did or it is not a reasoning blob
/// this protocol wrote.
pub fn items_for(provider: &str, blob: &str) -> Vec<String> {
    if !blob.starts_with('{') {
        return match provider == crate::codex::PROVIDER_NAME && !blob.is_empty() {
            true => vec![blob.to_string()],
            false => Vec::new(),
        };
    }
    let Ok(value) = serde_json::from_str::<Value>(blob) else {
        return Vec::new();
    };
    let Some(sealed) = value.get(KEY) else {
        return Vec::new();
    };
    if sealed.get("provider").and_then(Value::as_str) != Some(provider) {
        return Vec::new();
    }
    sealed
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_blob_opens_only_for_its_own_provider() {
        let blob = seal("meta", &["a".to_string(), "b".to_string()]).unwrap();
        assert_eq!(items_for("meta", &blob), ["a", "b"]);
        assert!(items_for("xai", &blob).is_empty());
        assert!(items_for("codex", &blob).is_empty());
    }

    #[test]
    fn nothing_to_keep_seals_to_nothing() {
        assert_eq!(seal("xai", &[]), None);
    }

    #[test]
    fn a_bare_item_belongs_to_codex_alone() {
        assert_eq!(items_for("codex", "sealed-blob"), ["sealed-blob"]);
        assert!(items_for("xai", "sealed-blob").is_empty());
        assert!(items_for("codex", "").is_empty());
    }

    #[test]
    fn another_protocols_blob_is_never_read_as_reasoning() {
        for blob in [
            r#"{"bedrock":[{"reasoningContent":{}}]}"#,
            "{not json",
            r#"{"responses":{"provider":"meta","items":"not a list"}}"#,
        ] {
            assert!(items_for("meta", blob).is_empty(), "{blob}");
        }
        let mixed = r#"{"responses":{"provider":"meta","items":["ok",7]}}"#;
        assert_eq!(items_for("meta", mixed), ["ok"]);
    }
}
