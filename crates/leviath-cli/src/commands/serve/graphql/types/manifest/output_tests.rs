//! Tests for what a run or a stage is asked to hand back, and the mirrors of
//! those types.

use super::{OutputArtifact, OutputSpec, StageParts, ValidatorErrorPolicy, ValidatorTool};
use crate::commands::serve::graphql::filter::testkit::{exercise, exercise_enum, exercise_list};
use crate::commands::serve::graphql::scalars::Json;

/// One artifact slot, as a manifest would declare it.
fn artifact() -> OutputArtifact {
    OutputArtifact {
        name: "report".to_string(),
        mime_type: "text/markdown".to_string(),
        required: true,
        description: Some("the write-up".to_string()),
    }
}

/// A validator tool, as a manifest would declare it.
fn checker() -> ValidatorTool {
    ValidatorTool {
        tool: "pw__check_panel".to_string(),
        content_arg: "submission".to_string(),
        args: Json(serde_json::json!({ "strict": true })),
    }
}

/// Every function `#[mirror]` wrote for this file's types runs at least once.
///
/// The mirrors are straight lines of delegation, so running each of them once
/// is enough to measure all of them.
#[tokio::test]
async fn every_mirrored_function_runs() {
    exercise_enum(&[ValidatorErrorPolicy::Reject, ValidatorErrorPolicy::Accept]).await;

    exercise(&[artifact()]).await;
    exercise_list(&[artifact()]).await;

    exercise(&[OutputSpec {
        format: Some("json".to_string()),
        instructions: Some("one object per finding".to_string()),
        example: Some("{}".to_string()),
        schema: Some(Json(serde_json::json!({ "type": "object" }))),
        validator: Some("checks/output.rhai".to_string()),
        on_validator_error: Some(ValidatorErrorPolicy::Accept),
        validator_tool: Some(checker()),
        overwrite_artifacts: Some(true),
        artifacts: vec![artifact()],
    }])
    .await;
    exercise(&[checker()]).await;

    exercise(&[StageParts {
        accepts: vec!["image/*".to_string()],
        as_text: vec!["text/plain".to_string()],
    }])
    .await;
}

/// A graph's output shape carries the artifacts and the validator policy
/// across, not just the top-level strings, and a validator is served as the
/// file it names or the code written inline.
#[test]
fn the_conversion_carries_artifacts_and_the_validator_policy() {
    use leviath_core::output::OnValidatorError;
    use leviath_runtime::spec::graph::{ArtifactDef, CodeRef, OutputDef};
    use leviath_runtime::spec::names::MimePattern;

    let def = OutputDef {
        format: Some("json".to_string()),
        schema: Some(leviath_core::JsonDoc::new(
            serde_json::json!({ "type": "object" }),
        )),
        validator: Some(CodeRef::File("checks/output.rhai".to_string())),
        on_validator_error: Some(OnValidatorError::Accept),
        artifacts: vec![ArtifactDef {
            name: "report".to_string(),
            mime_type: MimePattern::new("text/markdown").unwrap(),
            required: true,
            description: None,
        }],
        ..OutputDef::default()
    };
    let mapped = OutputSpec::from(&def);
    assert_eq!(mapped.format, Some("json".to_string()));
    assert_eq!(
        mapped.on_validator_error,
        Some(ValidatorErrorPolicy::Accept)
    );
    assert_eq!(mapped.validator.as_deref(), Some("checks/output.rhai"));
    assert_eq!(mapped.schema.unwrap().0["type"], "object");
    assert_eq!(mapped.artifacts.len(), 1);
    assert_eq!(mapped.artifacts[0].name, "report");
    assert_eq!(mapped.artifacts[0].mime_type, "text/markdown");

    let inline = OutputDef {
        validator: Some(CodeRef::Inline("fn validate(o) { true }".to_string())),
        ..OutputDef::default()
    };
    assert_eq!(
        OutputSpec::from(&inline).validator.as_deref(),
        Some("fn validate(o) { true }")
    );
    assert!(OutputSpec::from(&inline).validator_tool.is_none());

    let checked = OutputDef {
        validator_tool: Some(leviath_runtime::spec::graph::ValidatorToolDef {
            tool: leviath_runtime::spec::names::ToolName::new("pw__check_panel").unwrap(),
            content_arg: "submission".to_string(),
            args: leviath_core::JsonDoc::new(serde_json::json!({ "strict": true })),
        }),
        ..OutputDef::default()
    };
    let tool = OutputSpec::from(&checked).validator_tool.expect("carried");
    assert_eq!(tool.tool, "pw__check_panel");
    assert_eq!(tool.content_arg, "submission");
    assert_eq!(tool.args.0, serde_json::json!({ "strict": true }));
}
