//! A JSON document held as a typed value.
//!
//! Some values really are JSON: a tool's parameter schema, the arguments a
//! model wrote for a tool call, the JSON Schema a caller hands in for an
//! answer. [`JsonDoc`] is the one type for them.
//!
//! In a human-readable format (JSON, TOML) it is the document itself, nested in
//! place. In a binary format (the run file's postcard frames, which cannot
//! carry a self-describing value) it travels as the document's canonical text
//! and is parsed back on read, so a corrupt document fails the read loudly
//! rather than surfacing later as a surprise.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A JSON document. See the module docs for how it is encoded.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JsonDoc(serde_json::Value);

impl JsonDoc {
    /// Wrap a parsed document.
    pub fn new(value: serde_json::Value) -> Self {
        Self(value)
    }

    /// Parse a document from its text.
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text).map(Self)
    }

    /// The document.
    pub fn value(&self) -> &serde_json::Value {
        &self.0
    }

    /// The document, owned.
    pub fn into_value(self) -> serde_json::Value {
        self.0
    }

    /// The document's compact text.
    pub fn to_text(&self) -> String {
        self.0.to_string()
    }
}

impl From<serde_json::Value> for JsonDoc {
    fn from(value: serde_json::Value) -> Self {
        Self(value)
    }
}

impl schemars::JsonSchema for JsonDoc {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "JsonDoc".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "description": "Any JSON document." })
    }
}

impl Serialize for JsonDoc {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            self.0.serialize(s)
        } else {
            s.serialize_str(&self.to_text())
        }
    }
}

impl<'de> Deserialize<'de> for JsonDoc {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if d.is_human_readable() {
            serde_json::Value::deserialize(d).map(Self)
        } else {
            let text = String::deserialize(d)?;
            Self::parse(&text).map_err(|e| D::Error::custom(format!("stored JSON document: {e}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_readable_format_nests_the_document_in_place() {
        let doc = JsonDoc::new(json!({"type": "object"}));
        let text = serde_json::to_string(&doc).unwrap();
        assert_eq!(text, r#"{"type":"object"}"#);
        let back: JsonDoc = serde_json::from_str(&text).unwrap();
        assert_eq!(back, doc);
    }

    #[test]
    fn a_binary_format_carries_the_text_and_parses_it_back() {
        let doc = JsonDoc::from(json!({"a": [1, 2, {"b": null}]}));
        let bytes = postcard::to_stdvec(&doc).unwrap();
        let back: JsonDoc = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, doc);
        assert_eq!(back.into_value(), json!({"a": [1, 2, {"b": null}]}));
    }

    #[test]
    fn a_corrupt_stored_document_fails_the_read() {
        let bytes = postcard::to_stdvec("{not json").unwrap();
        let err = postcard::from_bytes::<JsonDoc>(&bytes).unwrap_err();
        assert!(
            err.to_string().contains("Serde Deserialization Error"),
            "{err}"
        );
    }

    #[test]
    fn text_round_trips() {
        let doc = JsonDoc::parse(r#"{"k": "v"}"#).unwrap();
        assert_eq!(doc.to_text(), r#"{"k":"v"}"#);
        assert_eq!(doc.value(), &json!({"k": "v"}));
        assert!(JsonDoc::parse("nope").is_err());
        assert_eq!(JsonDoc::default().value(), &serde_json::Value::Null);
    }
}
