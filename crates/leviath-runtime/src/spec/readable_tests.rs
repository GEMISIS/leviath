use super::*;
use crate::spec::graph::{InteractiveDef, StageMode, WorkerSource};
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
        json!({"kind": "list", "item": "text", "max": 3}),
        InputType::List {
            item: Box::new(text.clone()),
            min: None,
            max: Some(3),
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
    refused::<InputType>(json!({"kind": "int", "max": 1.5}), "is a whole number");
    refused::<InputType>(
        json!({"kind": "list", "item": "text", "min": -1}),
        "is a count",
    );
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
            "compact_history",
            RegionKind::CompactHistory { source: None },
        ),
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
            source: Some(region("chat")),
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
    refused::<RegionKind>(json!({"kind": "custom"}), "needs `code`");
}

#[test]
fn a_short_form_of_the_wrong_shape_is_named() {
    use serde_json::json;
    refused::<Budget>(json!(-1), "is not a token count");
    refused::<Budget>(json!(true), "a number, a name or a table");
    refused::<OutputCap>(json!(u64::MAX), "is too large");
    refused::<OutputCap>(json!(-1), "is not a token count");
    assert_eq!(
        <KindText as schemars::JsonSchema>::schema_name(),
        "Short_KindTable"
    );
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

/// Each short form's published schema says what its reader takes: a whole
/// number only where one is read, the names that stand alone as an enum, the
/// table's `kind` as an enum, and a percentage as a pattern.
#[test]
fn each_short_form_schema_is_as_precise_as_its_reader() {
    use serde_json::{Value, json};
    let schema =
        serde_json::to_value(schemars::schema_for!(crate::spec::request::SpawnRequest)).unwrap();
    let defs = &schema["$defs"];
    let branches = |name: &str| -> Vec<Value> {
        defs[name]["anyOf"]
            .as_array()
            .unwrap_or_else(|| panic!("{name}: {}", defs[name]))
            .clone()
    };
    let typed = |name: &str, ty: &str| -> Vec<Value> {
        branches(name)
            .into_iter()
            .filter(|b| b["type"] == ty)
            .collect()
    };
    let names = |name: &str| -> Vec<String> {
        let found: Vec<Value> = branches(name)
            .into_iter()
            .filter(|b| b.get("enum").is_some())
            .collect();
        assert_eq!(found.len(), 1, "{name}: {found:?}");
        serde_json::from_value(found[0]["enum"].clone()).unwrap()
    };
    for name in ["InputType", "RegionKind", "InputSlot"] {
        assert!(typed(name, "integer").is_empty(), "{name} reads no number");
        assert!(
            typed(name, "string")
                .iter()
                .all(|b| b.get("enum").is_some()),
            "{name}: names, not any text"
        );
    }
    for ty in names("InputType") {
        serde_json::from_value::<InputType>(json!(ty)).unwrap();
    }
    assert!(names("InputType").contains(&"text".to_string()));
    assert!(!names("InputType").contains(&"choice".to_string()));
    for kind in names("RegionKind") {
        serde_json::from_value::<RegionKind>(json!(kind)).unwrap();
    }
    assert!(!names("RegionKind").contains(&"custom".to_string()));
    assert_eq!(names("InputSlot"), ["output_format", "output_instructions"]);
    let kinds = |table: &str| -> Vec<String> {
        serde_json::from_value(defs[table]["properties"]["kind"]["enum"].clone())
            .unwrap_or_else(|e| panic!("{table}: {e}"))
    };
    assert_eq!(kinds("TypeTable").join(", "), TYPE_NAMES);
    assert_eq!(kinds("KindTable").join(", "), KIND_NAMES);
    for name in ["Budget", "OutputCap"] {
        let whole = typed(name, "integer");
        assert_eq!(whole.len(), 1, "{name}");
        assert_eq!(whole[0]["minimum"], 0);
        assert_eq!(whole[0]["maximum"], u32::MAX);
        let text = typed(name, "string");
        assert!(text[0]["pattern"].is_string(), "{name}: {text:?}");
    }
    assert!(
        !serde_json::to_string(defs).unwrap().contains("NoTable"),
        "a reply cap has no table form"
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

fn interactive(idle: Option<u64>) -> StageMode {
    StageMode::Interactive(InteractiveDef {
        idle_timeout_secs: idle,
    })
}

/// A mode with no settings is its name; one with settings is a table under
/// its name. Both forms of an interactive stage read, and one with no
/// settings is written as the plain name it always was.
#[test]
fn stage_modes_are_a_name_or_a_table() {
    use serde_json::json;
    same(json!("autonomous"), StageMode::Autonomous);
    same(json!("output"), StageMode::Output);
    same(json!("interactive"), interactive(None));
    same(
        json!({"interactive": {"idle_timeout_secs": 900}}),
        interactive(Some(900)),
    );
    let table: StageMode = serde_json::from_value(json!({"interactive": {}})).unwrap();
    assert_eq!(table, interactive(None));
    let points: StageMode = serde_json::from_value(json!({"interactive_points": []})).unwrap();
    assert_eq!(points, StageMode::InteractivePoints(Vec::new()));
    // The way a blueprint writes them.
    #[derive(Deserialize)]
    struct Holder {
        mode: StageMode,
    }
    let toml_mode = |text: &str| toml::from_str::<Holder>(text).unwrap().mode;
    assert_eq!(toml_mode("mode = \"interactive\""), interactive(None));
    assert_eq!(
        toml_mode("mode = { interactive = { idle_timeout_secs = 900 } }"),
        interactive(Some(900))
    );
    assert_eq!(
        toml_mode("[mode.interactive]\nidle_timeout_secs = 60"),
        interactive(Some(60))
    );
}

/// The run file keeps the binary shape modes always had: an interactive
/// stage with no settings is the bare variant at its old place, so a file
/// written before the mode took settings reads the same. One with settings
/// is the variant added at the end.
#[test]
fn an_interactive_mode_keeps_its_binary_shape() {
    assert_eq!(postcard::to_stdvec(&interactive(None)).unwrap(), vec![1]);
    assert_eq!(
        postcard::from_bytes::<StageMode>(&[1]).unwrap(),
        interactive(None)
    );
    let set = postcard::to_stdvec(&interactive(Some(900))).unwrap();
    assert_eq!(set[0], 5);
    assert_eq!(
        postcard::from_bytes::<StageMode>(&set).unwrap(),
        interactive(Some(900))
    );
    let points = StageMode::InteractivePoints(Vec::new());
    let fan = StageMode::FanOut(crate::test_graph::fan_out_to("split", "work"));
    for mode in [StageMode::Autonomous, StageMode::Output, points, fan] {
        let bin = postcard::to_stdvec(&mode).unwrap();
        assert_eq!(postcard::from_bytes::<StageMode>(&bin).unwrap(), mode);
        let text = serde_json::to_value(&mode).unwrap();
        assert_eq!(serde_json::from_value::<StageMode>(text).unwrap(), mode);
    }
}

/// A stage's `prompt_regions` read and write in readable formats while the run
/// file keeps the fields a stage always had: a stage that names none is the
/// bytes it always was, and one that names some carries them in its mode's
/// slot, whatever the mode, and gets them back out.
#[test]
fn a_stages_prompt_regions_keep_the_binary_shape() {
    use crate::spec::graph::StageDef;
    let plain = crate::test_graph::stage("render");
    assert!(plain.prompt_regions.is_empty());
    let text = serde_json::to_value(&plain).unwrap();
    assert!(text.get("prompt_regions").is_none(), "omitted when empty");
    let plain_bin = postcard::to_stdvec(&plain).unwrap();
    assert_eq!(postcard::from_bytes::<StageDef>(&plain_bin).unwrap(), plain);

    let modes = [
        StageMode::Autonomous,
        StageMode::Output,
        interactive(None),
        interactive(Some(900)),
        StageMode::InteractivePoints(Vec::new()),
        StageMode::FanOut(crate::test_graph::fan_out_to("split", "work")),
    ];
    for mode in modes {
        let stage = StageDef {
            mode: mode.clone(),
            prompt_regions: vec![region("brief"), region("style")],
            ..plain.clone()
        };
        let bin = postcard::to_stdvec(&stage).unwrap();
        assert_eq!(postcard::from_bytes::<StageDef>(&bin).unwrap(), stage);
        let json = serde_json::to_value(&stage).unwrap();
        assert_eq!(
            json["prompt_regions"],
            serde_json::json!(["brief", "style"])
        );
        assert_eq!(serde_json::from_value::<StageDef>(json).unwrap(), stage);

        // The same stage without the setting is the shorter, older bytes.
        let bare = StageDef {
            mode,
            ..plain.clone()
        };
        let bare_bin = postcard::to_stdvec(&bare).unwrap();
        assert!(bare_bin.len() < bin.len());
        assert_eq!(postcard::from_bytes::<StageDef>(&bare_bin).unwrap(), bare);
    }
}

/// A model block's `effort` reads and writes in readable formats, while the
/// run file keeps the four fields a model block always had: a block with no
/// effort is the same bytes as before, and one with an effort keeps it among
/// its parameters and gets it back out.
#[test]
fn a_model_blocks_effort_keeps_the_binary_shape() {
    use crate::spec::graph::{ModelChoice, ModelParams};
    use leviath_core::Effort;
    use serde_json::json;
    #[derive(Serialize)]
    struct Before {
        models: Vec<crate::spec::names::ModelRef>,
        allow_user_default: bool,
        params: ModelParams,
        request_timeout_secs: Option<u64>,
    }
    let before = |params: ModelParams| Before {
        models: vec![crate::spec::names::ModelRef::parse("meta/muse-spark").unwrap()],
        allow_user_default: false,
        params,
        request_timeout_secs: Some(30),
    };
    let plain = ModelChoice {
        models: vec![crate::spec::names::ModelRef::parse("meta/muse-spark").unwrap()],
        allow_user_default: false,
        request_timeout_secs: Some(30),
        ..Default::default()
    };
    assert_eq!(
        postcard::to_stdvec(&plain).unwrap(),
        postcard::to_stdvec(&before(ModelParams::default())).unwrap()
    );
    let low = ModelChoice {
        effort: Some(Effort::Low),
        ..plain.clone()
    };
    let bin = postcard::to_stdvec(&low).unwrap();
    let mut folded = ModelParams::default();
    folded.extra.insert(
        super::choice::EFFORT_KEY.to_string(),
        ParamScalar::Text("low".into()),
    );
    assert_eq!(bin, postcard::to_stdvec(&before(folded)).unwrap());
    assert_eq!(postcard::from_bytes::<ModelChoice>(&bin).unwrap(), low);

    let text = serde_json::to_value(&low).unwrap();
    assert_eq!(text["effort"], json!("low"));
    assert_eq!(serde_json::from_value::<ModelChoice>(text).unwrap(), low);
    let unset = serde_json::to_value(&plain).unwrap();
    assert!(unset.get("effort").is_none());
    let toml: ModelChoice = toml::from_str("models = []\neffort = \"high\"").unwrap();
    assert_eq!(toml.effort, Some(Effort::High));

    refused::<ModelChoice>(json!({ "effort": "extreme" }), "unknown variant");
    refused::<ModelChoice>(
        json!({ "params": { "extra": { "leviath.effort": "low" } } }),
        "is not a parameter a stage may set",
    );
}

#[test]
fn a_mistyped_stage_mode_says_what_is_wrong() {
    use serde_json::json;
    refused::<StageMode>(json!("chatty"), "\"chatty\" is not a stage mode");
    refused::<StageMode>(json!(3), "\"3\" is not a stage mode");
    refused::<StageMode>(json!("fan_out"), "needs its settings");
    refused::<StageMode>(
        json!({"interactive": {"idle_timeout": 9}}),
        "unknown field `idle_timeout`",
    );
    let schema =
        serde_json::to_value(schemars::schema_for!(crate::spec::request::SpawnRequest)).unwrap();
    let modes = &schema["$defs"]["StageMode"]["anyOf"][0]["enum"];
    assert_eq!(modes, &json!(["autonomous", "interactive", "output"]));
}

/// An output shape's `validator_tool` reads and writes in readable formats,
/// while the run file keeps the eight fields an output shape always had: a
/// shape with no tool is the same bytes as before, and one with a tool keeps
/// it in its schema document and gets both back out.
#[test]
fn an_outputs_validator_tool_keeps_the_binary_shape() {
    use crate::spec::graph::{ArtifactDef, OutputDef, ValidatorToolDef};
    use leviath_core::JsonDoc;
    use leviath_core::output::OnValidatorError;
    use serde_json::json;
    #[derive(Serialize)]
    struct Before {
        format: Option<String>,
        instructions: Option<String>,
        example: Option<String>,
        schema: Option<JsonDoc>,
        validator: Option<CodeRef>,
        on_validator_error: Option<OnValidatorError>,
        overwrite_artifacts: Option<bool>,
        artifacts: Vec<ArtifactDef>,
    }
    let before = |schema: Option<serde_json::Value>| Before {
        format: Some("json".into()),
        instructions: None,
        example: None,
        schema: schema.map(JsonDoc::new),
        validator: None,
        on_validator_error: Some(OnValidatorError::Accept),
        overwrite_artifacts: None,
        artifacts: vec![],
    };
    let plain = OutputDef {
        format: Some("json".into()),
        schema: Some(JsonDoc::new(json!({"type": "object"}))),
        on_validator_error: Some(OnValidatorError::Accept),
        ..OutputDef::default()
    };
    assert_eq!(
        postcard::to_stdvec(&plain).unwrap(),
        postcard::to_stdvec(&before(Some(json!({"type": "object"})))).unwrap()
    );
    let tool = ValidatorToolDef {
        tool: ToolName::new("pw__check_panel").unwrap(),
        content_arg: "panel".into(),
        args: JsonDoc::new(json!({"strict": true})),
    };
    let checked = OutputDef {
        validator_tool: Some(tool.clone()),
        ..plain.clone()
    };
    let bin = postcard::to_stdvec(&checked).unwrap();
    let stored = json!({
        "leviath.validator_tool": {
            "tool": "pw__check_panel", "content_arg": "panel", "args": {"strict": true}
        },
        "leviath.schema": {"type": "object"},
    });
    assert_eq!(bin, postcard::to_stdvec(&before(Some(stored))).unwrap());
    assert_eq!(postcard::from_bytes::<OutputDef>(&bin).unwrap(), checked);

    // A tool with no schema beside it, and a schema that happens to use the
    // key with no tool, both come back as they went in.
    let bare = OutputDef {
        validator_tool: Some(tool),
        ..OutputDef::default()
    };
    let odd = OutputDef {
        schema: Some(JsonDoc::new(json!({"leviath.validator_tool": 1}))),
        ..OutputDef::default()
    };
    for def in [bare, odd] {
        let bin = postcard::to_stdvec(&def).unwrap();
        assert_eq!(postcard::from_bytes::<OutputDef>(&bin).unwrap(), def);
    }
    // A stored tool that does not read fails the read loudly.
    let corrupt = before(Some(json!({"leviath.validator_tool": {"tool": 3}})));
    let err = postcard::from_bytes::<OutputDef>(&postcard::to_stdvec(&corrupt).unwrap())
        .unwrap_err()
        .to_string();
    assert!(!err.is_empty());

    same(
        json!({
            "format": "json",
            "instructions": null,
            "example": null,
            "schema": {"type": "object"},
            "validator": null,
            "on_validator_error": "accept",
            "validator_tool": {
                "tool": "pw__check_panel", "content_arg": "panel", "args": {"strict": true}
            },
            "overwrite_artifacts": null,
            "artifacts": [],
        }),
        checked,
    );
    let toml: OutputDef = toml::from_str("validator_tool = { tool = \"check\" }").unwrap();
    let read = toml.validator_tool.unwrap();
    assert_eq!(read.content_arg, "content");
    assert_eq!(read.args.value(), &json!({}));
    refused::<OutputDef>(
        json!({"validator_tool": {"tool": "check", "extra": 1}}),
        "unknown field",
    );
}
