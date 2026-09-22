//! Tests for the canonical filter rendering and its size limits.

use async_graphql::indexmap::IndexMap;
use async_graphql::{InputObject, Name, Value, value};

use super::{MAX_FILTER_DEPTH, MAX_FILTER_NODES, canonical, canonical_value};

/// A filter mirror, standing in for a generated one.
#[derive(Debug, Default, InputObject)]
struct RowInput {
    /// The name, or nothing.
    name: Option<String>,
    /// How many, or nothing.
    count: Option<i32>,
}

/// Nest `depth` objects inside one another, each holding one field.
fn nested(depth: usize) -> Value {
    let mut value = Value::String("leaf".to_string());
    for _ in 0..depth {
        let mut fields = IndexMap::new();
        fields.insert(Name::new("and"), value);
        value = Value::Object(fields);
    }
    value
}

/// An object of `count` scalar fields.
fn wide(count: usize) -> Value {
    let mut fields = IndexMap::new();
    for at in 0..count {
        fields.insert(Name::new(format!("f{at}")), Value::Number(1.into()));
    }
    Value::Object(fields)
}

fn rendered(value: Value) -> String {
    canonical_value(&value).expect("within the limits")
}

/// The one that keeps REST and GraphQL cursors interchangeable: no filter at
/// all contributes nothing to the digest, not an empty part.
#[test]
fn an_empty_filter_renders_as_nothing() {
    assert_eq!(canonical(&RowInput::default()).expect("rendered"), "");
    assert_eq!(rendered(value!({})), "");
    assert_eq!(rendered(Value::Null), "");
    // An object whose every field is null is the same filter as no object.
    assert_eq!(
        rendered(value!({ "name": null, "inner": { "x": null } })),
        ""
    );
}

/// Leaving a field out and passing it as null are the same filter, so they
/// have to digest the same way.
#[test]
fn a_null_field_renders_the_same_as_an_absent_one() {
    let filled = RowInput {
        name: Some("build".to_string()),
        count: None,
    };
    assert_eq!(canonical(&filled).expect("rendered"), "{name:\"build\"}");
    assert_eq!(
        rendered(value!({ "name": "build", "count": null })),
        "{name:\"build\"}"
    );
}

/// Declaration order, not sorted order: the value already arrives in the order
/// the type declares, and re-sorting would only hide a field moving.
#[test]
fn fields_keep_the_order_the_filter_declares() {
    assert_eq!(rendered(value!({ "b": 1, "a": 2 })), "{b:1,a:2}");
    assert_ne!(
        rendered(value!({ "a": 2, "b": 1 })),
        rendered(value!({ "b": 1, "a": 2 }))
    );
}

#[test]
fn every_scalar_shape_renders_to_something_a_digest_can_separate() {
    assert_eq!(rendered(value!({ "n": 7 })), "{n:7}");
    assert_eq!(
        rendered(value!({ "yes": true, "no": false })),
        "{yes:true,no:false}"
    );
    assert_eq!(
        rendered(Value::Object(IndexMap::from_iter([(
            Name::new("kind"),
            Value::Enum(Name::new("RUNNING")),
        )]))),
        "{kind:RUNNING}"
    );
    assert_eq!(
        rendered(Value::Object(IndexMap::from_iter([(
            Name::new("blob"),
            Value::Binary(vec![0xde, 0xad].into()),
        )]))),
        "{blob:dead}"
    );
}

/// A value that contains the punctuation this rendering uses must not be able
/// to look like more of it.
#[test]
fn a_string_is_quoted_and_its_own_quotes_escaped() {
    assert_eq!(rendered(value!({ "q": "a,b" })), "{q:\"a,b\"}");
    assert_eq!(
        rendered(value!({ "q": "he said \"hi\"" })),
        "{q:\"he said \\\"hi\\\"\"}"
    );
    assert_eq!(
        rendered(value!({ "q": "back\\slash" })),
        "{q:\"back\\\\slash\"}"
    );
    // An empty string is a filter; it must not vanish the way a null does.
    assert_eq!(rendered(value!({ "q": "" })), "{q:\"\"}");
}

/// `in: []` matches nothing and `in: null` matches everything, so the two
/// cannot render alike - and neither can `[null]` and `[]`.
#[test]
fn a_list_keeps_its_length_and_its_holes() {
    assert_eq!(rendered(value!({ "ids": [] })), "{ids:[]}");
    assert_eq!(
        rendered(value!({ "ids": ["a", "b"] })),
        "{ids:[\"a\",\"b\"]}"
    );
    assert_eq!(rendered(value!({ "ids": [null] })), "{ids:[null]}");
    assert_ne!(
        rendered(value!({ "ids": [] })),
        rendered(value!({ "ids": [null] }))
    );
}

#[test]
fn a_filter_at_the_depth_limit_is_rendered_and_one_past_it_is_refused() {
    // The outermost value is depth one, so sixteen levels is fifteen nestings.
    assert!(canonical_value(&nested(MAX_FILTER_DEPTH - 1)).is_ok());
    let refusal = canonical_value(&nested(MAX_FILTER_DEPTH)).expect_err("refused");
    assert_eq!(refusal.code(), "BAD_USER_INPUT");
    assert!(refusal.to_string().contains("nests more than 16"));
}

#[test]
fn a_filter_at_the_node_limit_is_rendered_and_one_past_it_is_refused() {
    // The object itself is one node, so it may hold one fewer field.
    assert!(canonical_value(&wide(MAX_FILTER_NODES - 1)).is_ok());
    let refusal = canonical_value(&wide(MAX_FILTER_NODES)).expect_err("refused");
    assert_eq!(refusal.code(), "BAD_USER_INPUT");
    assert!(refusal.to_string().contains("more than 512 values"));
}

/// A list is counted too, so a wide filter cannot hide its size in one field.
#[test]
fn list_elements_count_against_the_node_limit() {
    let many = Value::List((0..MAX_FILTER_NODES).map(|_| Value::Null).collect());
    assert!(canonical_value(&many).is_err());
}
