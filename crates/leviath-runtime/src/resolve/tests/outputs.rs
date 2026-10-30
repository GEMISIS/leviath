use leviath_core::JsonDoc;
use leviath_core::output::OnValidatorError;

use super::*;
use crate::resolve::output::cascade;
use crate::spec::graph::{ArtifactDef, OutputDef};

fn artifact(name: &str) -> ArtifactDef {
    ArtifactDef {
        name: name.into(),
        mime_type: n("image/png"),
        required: true,
        description: None,
    }
}

fn declared() -> OutputDef {
    OutputDef {
        format: Some("json".into()),
        instructions: Some("be brief".into()),
        example: Some("{}".into()),
        schema: Some(JsonDoc::new(serde_json::json!({"type": "object"}))),
        validator: Some(CodeRef::Inline("fn validate(x) {}".into())),
        on_validator_error: Some(OnValidatorError::Accept),
        validator_tool: Some(crate::spec::graph::ValidatorToolDef {
            tool: crate::spec::names::ToolName::new("check_panel").unwrap(),
            content_arg: "content".into(),
            args: JsonDoc::new(serde_json::json!({})),
        }),
        overwrite_artifacts: Some(false),
        artifacts: vec![artifact("chart")],
    }
}

#[test]
fn no_level_asking_for_output_means_no_output() {
    assert_eq!(cascade(None, None, None), None);
}

#[test]
fn later_levels_win_field_by_field() {
    let stage = OutputDef {
        instructions: Some("be thorough".into()),
        artifacts: vec![artifact("table")],
        ..OutputDef::default()
    };
    let request = OutputDef {
        example: Some("{\"a\": 1}".into()),
        overwrite_artifacts: Some(true),
        ..OutputDef::default()
    };
    let out = cascade(Some(&declared()), Some(&stage), Some(&request)).unwrap();
    assert_eq!(out.format.as_deref(), Some("json"));
    assert_eq!(out.instructions.as_deref(), Some("be thorough"));
    assert_eq!(out.example.as_deref(), Some("{\"a\": 1}"));
    assert!(out.schema.is_some());
    assert!(out.validator.is_some());
    assert!(out.validator_tool.is_some());
    assert_eq!(out.on_validator_error, Some(OnValidatorError::Accept));
    assert_eq!(out.overwrite_artifacts, Some(true));
    assert_eq!(
        out.artifacts,
        [artifact("table")],
        "the nearest list wins whole"
    );
}

#[test]
fn a_caller_asking_for_the_same_format_keeps_the_checks() {
    let request = OutputDef {
        format: Some("json".into()),
        ..OutputDef::default()
    };
    let out = cascade(None, Some(&declared()), Some(&request)).unwrap();
    assert!(out.schema.is_some());
    assert_eq!(out.artifacts, [artifact("chart")]);
}

#[test]
fn a_caller_asking_for_another_format_retires_the_declared_checks() {
    let request = OutputDef {
        format: Some("a2ui".into()),
        ..OutputDef::default()
    };
    let out = cascade(Some(&declared()), None, Some(&request)).unwrap();
    assert_eq!(out.format.as_deref(), Some("a2ui"));
    assert_eq!(out.instructions.as_deref(), Some("be brief"));
    assert_eq!(out.schema, None);
    assert_eq!(out.validator, None);
    assert_eq!(out.validator_tool, None);
    assert_eq!(out.on_validator_error, None);
    assert!(out.artifacts.is_empty());

    let with_own = OutputDef {
        artifacts: vec![artifact("ui")],
        ..request
    };
    let out = cascade(Some(&declared()), None, Some(&with_own)).unwrap();
    assert_eq!(out.artifacts, [artifact("ui")]);
}

#[tokio::test]
async fn the_request_output_is_kept_as_asked_and_applied_to_every_stage() {
    let mut request = raw(graph());
    request.output = Some(OutputDef {
        format: Some("markdown".into()),
        artifacts: vec![artifact("chart")],
        schema: Some(JsonDoc::new(serde_json::json!({"type": "string"}))),
        ..OutputDef::default()
    });
    let resolved = spawn(&request, &Fake::default()).await.unwrap();
    assert_eq!(resolved.spec.requested_output, request.output);
    for stage in &resolved.spec.stages {
        assert_eq!(stage.output, request.output);
    }
}

/// An output spec's validator tool reads into the run's form when its name
/// is a tool's name, and is an issue at its place when it is not.
#[test]
fn an_output_specs_validator_tool_is_named_like_a_tool() {
    let mut spec = leviath_core::output::OutputSpec {
        validator_tool: Some(leviath_core::output::ValidatorTool {
            tool: "not a tool".into(),
            content_arg: "panel".into(),
            args: serde_json::Map::from_iter([("strict".into(), serde_json::json!(true))]),
        }),
        ..Default::default()
    };
    let issues = OutputDef::from_output_spec(&spec).unwrap_err();
    assert_eq!(found(&issues), ["output.validator_tool.tool Invalid"]);
    if let Some(tool) = spec.validator_tool.as_mut() {
        tool.tool = "check".into();
    }
    let def = OutputDef::from_output_spec(&spec).unwrap();
    assert_eq!(def.validator_tool.map(|t| t.spec()), spec.validator_tool);
}

/// A validator tool named `tool`, its arguments `args`, the answer under
/// `content_arg`.
fn checker(tool: &str, content_arg: &str, args: serde_json::Value) -> OutputDef {
    OutputDef {
        validator_tool: Some(crate::spec::graph::ValidatorToolDef {
            tool: n(tool),
            content_arg: content_arg.into(),
            args: JsonDoc::new(args),
        }),
        ..OutputDef::default()
    }
}

/// A stage whose answers a tool judges must be given that tool: one the
/// machine does not offer the stage is refused at spawn, with what it does
/// offer, and so are arguments that are not an object, that take the
/// answer's place, or a blank place for it. A stage that never submits is
/// never asked about.
#[tokio::test]
async fn a_validator_tool_must_be_one_the_stage_is_given() {
    let submits = vec![
        tool("read_file"),
        tool("pw__check_panel"),
        tool(leviath_tools::SUBMIT_OUTPUT_TOOL),
    ];
    let env = || Fake {
        tools: [
            ("plan".to_string(), Ok(vec![tool("read_file")])),
            ("build".to_string(), Ok(submits.clone())),
        ]
        .into(),
        ..Fake::default()
    };
    let mut g = graph();
    g.output = Some(checker("pw__check_panel", "panel", serde_json::json!({})));
    let resolved = spawn(&raw(g.clone()), &env()).await.unwrap();
    let tool_of = |i: usize| {
        resolved.spec.stages[i]
            .output
            .as_ref()
            .and_then(|o| o.validator_tool.clone())
    };
    assert!(tool_of(0).is_some() && tool_of(1).is_some());

    g.output = Some(checker("pw__missing", "panel", serde_json::json!({})));
    let issues = spawn(&raw(g.clone()), &env()).await.unwrap_err();
    assert_eq!(
        found(&issues),
        ["source.raw.stages.build.output.validator_tool.tool Unresolvable"]
    );
    assert!(issues.0[0].known.iter().any(|k| k == "pw__check_panel"));

    for (content_arg, args, field) in [
        ("panel", serde_json::json!([1]), "args"),
        ("panel", serde_json::json!({"panel": "x"}), "args"),
        (" ", serde_json::json!({}), "content_arg"),
    ] {
        g.output = Some(checker("pw__check_panel", content_arg, args));
        let issues = spawn(&raw(g.clone()), &env()).await.unwrap_err();
        assert_eq!(
            found(&issues),
            [format!(
                "source.raw.stages.build.output.validator_tool.{field} Invalid"
            )]
        );
    }
}

/// A stage whose schema is an object offers `submit_output` with the
/// schema's fields among its parameters, and says in the description that
/// they go there or as one JSON object in `content`. A schema of any other
/// kind keeps the parameters the tool came with, and says `content`.
#[tokio::test]
async fn an_object_schema_shapes_the_submit_tools_parameters_and_description() {
    let submit = ToolDef {
        schema: JsonDoc::new(serde_json::json!({
            "type": "object",
            "properties": {"content": {"type": "string"}},
            "required": ["content"]
        })),
        ..tool(leviath_tools::SUBMIT_OUTPUT_TOOL)
    };
    let env = || Fake {
        tools: [
            ("plan".to_string(), Ok(vec![submit.clone()])),
            ("build".to_string(), Ok(vec![submit.clone()])),
        ]
        .into(),
        ..Fake::default()
    };
    let offered = |resolved: &Resolved| resolved.spec.stages[0].tools[0].clone();
    let mut g = graph();
    g.output = Some(OutputDef {
        format: Some("json".into()),
        schema: Some(JsonDoc::new(serde_json::json!({
            "type": "object",
            "properties": {"summary": {"type": "string"}},
            "required": ["summary"]
        }))),
        ..OutputDef::default()
    });
    let shaped = offered(&spawn(&raw(g.clone()), &env()).await.unwrap());
    let parameters = shaped.schema.value();
    assert!(
        parameters["properties"]["summary"].is_object(),
        "{parameters}"
    );
    assert!(
        parameters["properties"]["content"].is_object(),
        "{parameters}"
    );
    assert!(parameters.get("required").is_none(), "{parameters}");
    assert!(
        shaped.description.contains(
            "Give its properties as the arguments of `submit_output`, or the whole JSON object \
             as the text of `content`."
        ),
        "{}",
        shaped.description
    );

    g.output = Some(OutputDef {
        schema: Some(JsonDoc::new(serde_json::json!({"type": "array"}))),
        ..OutputDef::default()
    });
    let listed = offered(&spawn(&raw(g), &env()).await.unwrap());
    assert_eq!(listed.schema, submit.schema);
    assert!(
        listed
            .description
            .contains("Give the whole JSON document as the text of `content`."),
        "{}",
        listed.description
    );
}
