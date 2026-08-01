use super::*;
use serde_json::json;

fn invalid_message(v: ArgValidation) -> String {
    match v {
        ArgValidation::Invalid(m) => m,
        other => panic!("expected Invalid, got {other:?}"),
    }
}

fn unusable_reason(v: ArgValidation) -> String {
    match v {
        ArgValidation::SchemaUnusable(m) => m,
        other => panic!("expected SchemaUnusable, got {other:?}"),
    }
}

/// A builtin-style schema: one required string property.
fn read_file_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": { "path": { "type": "string" } },
        "required": ["path"]
    })
}

#[test]
fn valid_arguments_pass() {
    let v = validate_tool_args("read_file", &read_file_schema(), &json!({"path": "a.txt"}));
    assert_eq!(v, ArgValidation::Valid);
}

#[test]
fn missing_required_property_is_refused_with_tool_name_and_property() {
    let msg = invalid_message(validate_tool_args(
        "read_file",
        &read_file_schema(),
        &json!({}),
    ));
    assert!(
        msg.starts_with("[error] invalid arguments for 'read_file': "),
        "{msg}"
    );
    assert!(msg.contains("path"), "{msg}");
}

#[test]
fn wrong_type_is_refused_with_the_argument_path() {
    let msg = invalid_message(validate_tool_args(
        "read_file",
        &read_file_schema(),
        &json!({"path": 42}),
    ));
    assert!(msg.contains("at /path:"), "{msg}");
    assert!(msg.contains("42"), "{msg}");
}

#[test]
fn non_object_arguments_are_refused_at_the_root() {
    // A root-level violation renders without an `at <path>:` clause.
    let msg = invalid_message(validate_tool_args(
        "read_file",
        &read_file_schema(),
        &json!("just a string"),
    ));
    assert!(!msg.contains("at /"), "{msg}");
    assert!(msg.contains("object"), "{msg}");
}

#[test]
fn violations_beyond_the_cap_are_summarised() {
    let schema = json!({
        "type": "object",
        "properties": {
            "a": {"type": "string"}, "b": {"type": "string"}, "c": {"type": "string"},
            "d": {"type": "string"}, "e": {"type": "string"}
        },
        "required": ["a", "b", "c", "d", "e"]
    });
    let msg = invalid_message(validate_tool_args("many", &schema, &json!({})));
    assert!(msg.ends_with("; (and 2 more)"), "{msg}");
}

#[test]
fn violations_within_the_cap_have_no_summary_suffix() {
    let msg = invalid_message(validate_tool_args(
        "read_file",
        &read_file_schema(),
        &json!({}),
    ));
    assert!(!msg.contains("more)"), "{msg}");
}

#[test]
fn oversized_violation_messages_are_length_capped() {
    // The validator's message embeds the offending value; a huge argument must
    // not be echoed back in full. 4 KiB of string against an integer schema
    // renders as a message capped well below the input size.
    let schema = json!({
        "type": "object",
        "properties": { "n": { "type": "integer" } }
    });
    let huge = "x".repeat(4096);
    let msg = invalid_message(validate_tool_args("sized", &schema, &json!({ "n": huge })));
    assert!(
        msg.len() < 1024,
        "expected a capped message, got {} bytes",
        msg.len()
    );
}

#[test]
fn a_schema_that_does_not_compile_is_reported_unusable() {
    // A typo'd Rhai `@param n strng required` compiles to exactly this shape.
    let reason = unusable_reason(validate_tool_args(
        "typo",
        &json!({"type": "strng"}),
        &json!({"anything": true}),
    ));
    assert!(!reason.is_empty());
}

#[test]
fn an_external_ref_does_not_resolve_and_is_reported_unusable() {
    // With `default-features = false` an external `$ref` must fail to compile
    // instead of fetching over the network at validation time.
    let reason = unusable_reason(validate_tool_args(
        "remote",
        &json!({"$ref": "http://example.com/schema.json"}),
        &json!({}),
    ));
    assert!(!reason.is_empty());
}

#[test]
fn an_empty_schema_accepts_anything() {
    // Test fixtures and tools without declared parameters advertise `{}`,
    // which constrains nothing.
    let v = validate_tool_args("anything", &json!({}), &json!({"whatever": [1, 2, 3]}));
    assert_eq!(v, ArgValidation::Valid);
}

#[test]
fn null_arguments_normalise_to_an_empty_object() {
    let v = validate_tool_args("anything", &json!({}), &serde_json::Value::Null);
    assert_eq!(v, ArgValidation::Valid);
}

#[test]
fn null_arguments_still_fail_a_schema_with_required_properties() {
    let msg = invalid_message(validate_tool_args(
        "read_file",
        &read_file_schema(),
        &serde_json::Value::Null,
    ));
    assert!(msg.contains("path"), "{msg}");
}

#[test]
fn nested_mcp_style_schemas_validate_both_ways() {
    // The kind of schema an MCP server can send: enums, arrays with typed
    // items, a nested object.
    let schema = json!({
        "type": "object",
        "properties": {
            "mode": { "enum": ["fast", "thorough"] },
            "targets": { "type": "array", "items": { "type": "string" } },
            "options": {
                "type": "object",
                "properties": { "depth": { "type": "integer" } }
            }
        },
        "required": ["mode"]
    });
    let ok = validate_tool_args(
        "mcp_tool",
        &schema,
        &json!({"mode": "fast", "targets": ["a", "b"], "options": {"depth": 2}}),
    );
    assert_eq!(ok, ArgValidation::Valid);
    let msg = invalid_message(validate_tool_args(
        "mcp_tool",
        &schema,
        &json!({"mode": "sideways", "targets": ["a", 7]}),
    ));
    assert!(msg.contains("at /mode:"), "{msg}");
    assert!(msg.contains("at /targets/1:"), "{msg}");
}

#[test]
fn extra_properties_are_allowed_unless_the_schema_forbids_them() {
    // Plain properties/required schemas (every builtin def) tolerate extras;
    // that leniency is standard JSON Schema and deliberate here.
    let v = validate_tool_args(
        "read_file",
        &read_file_schema(),
        &json!({"path": "a.txt", "surprise": true}),
    );
    assert_eq!(v, ArgValidation::Valid);
}
