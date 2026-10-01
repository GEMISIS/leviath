use super::*;
use crate::spec::graph::WorkerSource;
use serde::de::DeserializeOwned;
use std::fmt::Debug;

/// `json` reads as `value`, `value` writes back as `json`, and the binary
/// form round-trips too.
fn same<T>(json: serde_json::Value, value: T)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let read: T = serde_json::from_value(json.clone()).unwrap_or_else(|e| panic!("{json}: {e}"));
    assert_eq!(read, value, "{json}");
    assert_eq!(serde_json::to_value(&value).unwrap(), json);
    let bin = postcard::to_stdvec(&value).unwrap();
    assert_eq!(postcard::from_bytes::<T>(&bin).unwrap(), value);
}

fn refused<T: DeserializeOwned + Debug>(json: serde_json::Value, words: &str) {
    let err = serde_json::from_value::<T>(json.clone())
        .unwrap_err()
        .to_string();
    assert!(err.contains(words), "{json}: {err}");
}

fn region(s: &str) -> RegionName {
    RegionName::new(s).unwrap()
}

fn stage(s: &str) -> StageName {
    StageName::new(s).unwrap()
}

#[test]
fn tool_selectors_are_names_or_at_groups() {
    use serde_json::json;
    same(
        json!("read_file"),
        ToolSelector::Tool(ToolName::new("read_file").unwrap()),
    );
    for (text, g) in [
        ("@all", ToolGroup::All),
        ("@builtin", ToolGroup::Builtin),
        ("@subagent", ToolGroup::Subagent),
        ("@scripts", ToolGroup::Scripts),
        ("@mcp", ToolGroup::Mcp),
    ] {
        same(json!(text), ToolSelector::Group(g));
    }
    refused::<ToolSelector>(json!("@everything"), "is not a tool group");
    refused::<ToolSelector>(json!("no spaces"), "uses only letters");
}

#[test]
fn model_settings_are_plain_values() {
    use serde_json::json;
    same(json!(true), ParamScalar::Bool(true));
    same(json!(3), ParamScalar::Int(3));
    same(json!(0.9), ParamScalar::Float(0.9));
    same(json!("high"), ParamScalar::Text("high".into()));
    same(
        json!(["a", "b"]),
        ParamScalar::TextList(vec!["a".into(), "b".into()]),
    );
}

#[test]
fn budgets_are_tokens_percentages_or_clamped_percentages() {
    use serde_json::json;
    same(json!(4000), Budget::Tokens(4000));
    same(
        json!("35%"),
        Budget::Percent {
            percent: 0.35,
            min: None,
            max: None,
        },
    );
    same(
        json!("100%"),
        Budget::Percent {
            percent: 1.0,
            min: None,
            max: None,
        },
    );
    same(
        json!("0.5%"),
        Budget::Percent {
            percent: 0.005,
            min: None,
            max: None,
        },
    );
    same(
        json!({"percent": "10%", "min": 500}),
        Budget::Percent {
            percent: 0.1,
            min: Some(500),
            max: None,
        },
    );
    same(
        json!({"percent": "10%", "min": 500, "max": 8000}),
        Budget::Percent {
            percent: 0.1,
            min: Some(500),
            max: Some(8000),
        },
    );
    refused::<Budget>(json!("ten"), "is not a percentage");
    refused::<Budget>(json!("x%"), "is not a percentage");
    refused::<Budget>(json!({"percent": "bad"}), "is not a percentage");
}

#[test]
fn output_caps_name_tokens_the_window_or_a_region() {
    use serde_json::json;
    same(json!(8000), OutputCap::Tokens(8000));
    same(json!("40%"), OutputCap::WindowPercent(0.4));
    same(
        json!("100% of claims"),
        OutputCap::RegionPercent {
            percent: 1.0,
            region: region("claims"),
        },
    );
    refused::<OutputCap>(json!("lots"), "is not a percentage");
    refused::<OutputCap>(json!("5% of "), "cannot be empty");
    refused::<OutputCap>(json!("a of b"), "is not a percentage");
}

#[test]
fn slots_name_their_target_by_key() {
    use serde_json::json;
    same(json!("output_format"), InputSlot::OutputFormat);
    same(json!("output_instructions"), InputSlot::OutputInstructions);
    same(
        json!({"region": "task"}),
        InputSlot::Region(RegionBinding {
            region: region("task"),
            template: None,
        }),
    );
    same(
        json!({"region": "task", "template": "Do {x}"}),
        InputSlot::Region(RegionBinding {
            region: region("task"),
            template: Some(Template::parse("Do {x}").unwrap()),
        }),
    );
    same(
        json!({"stage_model": "plan"}),
        InputSlot::StageModel(stage("plan")),
    );
    same(
        json!({"stage_max_iterations": "plan"}),
        InputSlot::StageMaxIterations(stage("plan")),
    );
    same(
        json!({"fan_out_max_workers": "plan"}),
        InputSlot::FanOutMaxWorkers(stage("plan")),
    );
    refused::<InputSlot>(json!("somewhere"), "is not a slot");
    refused::<InputSlot>(json!({}), "exactly one of");
    refused::<InputSlot>(json!({"region": "a", "stage_model": "b"}), "exactly one of");
    refused::<InputSlot>(
        json!({"stage_model": "b", "template": "t"}),
        "only a region slot",
    );
}

#[test]
fn input_types_are_a_name_or_a_kind_table() {
    use serde_json::json;
    let text = InputType::Text {
        multiline: false,
        min_len: None,
        max_len: None,
    };
    same(json!("text"), text.clone());
    same(
        json!({"kind": "text", "multiline": true, "max_len": 80}),
        InputType::Text {
            multiline: true,
            min_len: None,
            max_len: Some(80),
        },
    );
    same(json!("bool"), InputType::Bool);
    same(
        json!("int"),
        InputType::Int {
            min: None,
            max: None,
        },
    );
    same(
        json!({"kind": "int", "min": 1, "max": 5}),
        InputType::Int {
            min: Some(1),
            max: Some(5),
        },
    );
    same(
        json!("float"),
        InputType::Float {
            min: None,
            max: None,
        },
    );
    same(
        json!({"kind": "float", "min": 0.5}),
        InputType::Float {
            min: Some(0.5),
            max: None,
        },
    );
    same(
        json!({"kind": "choice", "options": ["a", "b"]}),
        InputType::Choice {
            options: vec![ChoiceName::new("a").unwrap(), ChoiceName::new("b").unwrap()],
        },
    );
    same(
        json!({"kind": "list", "item": "text", "min": 1}),
        InputType::List {
            item: Box::new(text.clone()),
            min: Some(1),
            max: None,
        },
    );
    same(
        json!({"kind": "record", "fields": [{"name": "a", "type": "bool", "required": false, "default": null, "description": null, "binds": []}]}),
        InputType::Record {
            fields: vec![InputDecl {
                name: crate::spec::names::InputName::new("a").unwrap(),
                ty: InputType::Bool,
                required: false,
                default: None,
                description: None,
                binds: vec![],
            }],
        },
    );
    same(json!("file"), InputType::File { accepts: vec![] });
    same(
        json!({"kind": "file", "accepts": ["image/*"]}),
        InputType::File {
            accepts: vec![MimePattern::new("image/*").unwrap()],
        },
    );
    same(
        json!({"kind": "path", "names": "dir", "must_exist": true}),
        InputType::Path {
            kind: PathKind::Dir,
            must_exist: true,
        },
    );
    same(json!("model"), InputType::Model);
    same(json!("blueprint"), InputType::Blueprint);
    same(json!("duration"), InputType::Duration);
    same(json!("url"), InputType::Url);
    // A bare `path` names anything.
    assert_eq!(
        serde_json::from_value::<InputType>(json!("path")).unwrap(),
        InputType::Path {
            kind: PathKind::Any,
            must_exist: false
        }
    );
    // A whole bound reads as a float bound for a float.
    assert_eq!(
        serde_json::from_value::<InputType>(json!({"kind": "float", "max": 2})).unwrap(),
        InputType::Float {
            min: None,
            max: Some(2.0)
        }
    );
}

#[test]
fn a_mistyped_input_type_says_what_is_wrong() {
    use serde_json::json;
    refused::<InputType>(json!("number"), "is not an input type");
    refused::<InputType>(
        json!({"kind": "bool", "min": 1}),
        "a bool input does not take `min`",
    );
    refused::<InputType>(json!({"kind": "choice"}), "needs `options`");
    refused::<InputType>(json!({"kind": "list"}), "needs `item`");
    refused::<InputType>(json!({"kind": "record"}), "needs `fields`");
    refused::<InputType>(json!({"kind": "int", "min": 1.5}), "is a whole number");
    refused::<InputType>(
        json!({"kind": "list", "item": "text", "max": -1}),
        "is a count",
    );
    refused::<InputType>(
        json!({"kind": "list", "item": "text", "max": 1.5}),
        "is a whole number",
    );
    refused::<InputType>(json!({"kind": "text", "colour": "red"}), "unknown field");
}

#[test]
fn region_kinds_are_a_name_or_a_kind_table() {
    use serde_json::json;
    for (text, kind) in [
        ("pinned", RegionKind::Pinned),
        ("temporary", RegionKind::Temporary),
        ("clearable", RegionKind::Clearable),
        ("checklist", RegionKind::Checklist),
        ("keyed", RegionKind::Keyed { max_entries: None }),
        (
            "compacting",
            RegionKind::Compacting {
                threshold_tokens: None,
            },
        ),
    ] {
        same(json!(text), kind);
    }
    same(
        json!({"kind": "sliding_window", "max_items": 20}),
        RegionKind::SlidingWindow {
            max_items: 20,
            eviction: Eviction::PerItem,
        },
    );
    same(
        json!({"kind": "sliding_window", "max_items": 20, "eviction": {"bulk": 5}}),
        RegionKind::SlidingWindow {
            max_items: 20,
            eviction: Eviction::Bulk(5),
        },
    );
    same(
        json!({"kind": "compacting", "threshold_tokens": 900}),
        RegionKind::Compacting {
            threshold_tokens: Some(900),
        },
    );
    same(
        json!({"kind": "compact_history", "source": "chat"}),
        RegionKind::CompactHistory {
            source: region("chat"),
        },
    );
    same(
        json!({"kind": "keyed", "max_entries": 9}),
        RegionKind::Keyed {
            max_entries: Some(9),
        },
    );
    same(
        json!({"kind": "custom", "code": {"file": "r.rhai"}, "pinned": true}),
        RegionKind::Custom {
            code: CodeRef::File("r.rhai".into()),
            pinned: true,
        },
    );
    refused::<RegionKind>(json!("sticky"), "is not a region kind");
    refused::<RegionKind>(
        json!({"kind": "pinned", "max_items": 3}),
        "does not take `max_items`",
    );
    refused::<RegionKind>(json!({"kind": "sliding_window"}), "needs `max_items`");
    refused::<RegionKind>(json!({"kind": "compact_history"}), "needs `source`");
    refused::<RegionKind>(json!({"kind": "custom"}), "needs `code`");
}

#[test]
fn a_short_form_of_the_wrong_shape_is_named() {
    use serde_json::json;
    refused::<Budget>(json!(-1), "is not a token count");
    refused::<Budget>(json!(true), "a number, a name or a table");
    refused::<OutputCap>(json!(u64::MAX), "is too large");
    refused::<OutputCap>(json!({"tokens": 3}), "takes a number or text, not a table");
    refused::<InputSlot>(json!(3), "is not a slot");
    refused::<InputType>(json!(3), "is not an input type");
    refused::<RegionKind>(json!(3), "is not a region kind");
    // A table keeps its own message, not "did not match any variant".
    refused::<InputSlot>(json!({"regoin": "task"}), "unknown field `regoin`");
    refused::<RegionKind>(
        json!({"kind": "keyed", "max_entry": 3}),
        "unknown field `max_entry`",
    );
}

#[test]
fn the_short_forms_publish_their_own_schema() {
    let schema =
        serde_json::to_string(&schemars::schema_for!(crate::spec::request::SpawnRequest)).unwrap();
    for word in [
        "ToolSelector",
        "Budget",
        "InputType",
        "RegionKind",
        "InputSlot",
        "OutputCap",
        "ParamScalar",
    ] {
        assert!(schema.contains(word), "{word}");
    }
    // A worker source is not a short-form type; it keeps its tagged form.
    let w = serde_json::to_value(WorkerSource::Stage(stage("plan"))).unwrap();
    assert_eq!(w, serde_json::json!({"stage": "plan"}));
}
