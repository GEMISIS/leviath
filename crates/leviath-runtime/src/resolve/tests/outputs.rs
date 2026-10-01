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
