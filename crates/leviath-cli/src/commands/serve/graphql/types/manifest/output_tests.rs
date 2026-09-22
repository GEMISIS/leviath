//! Tests for what a run or a stage is asked to hand back, and the mirrors of
//! those types.

use super::{OutputArtifact, OutputSpec, StageParts, ValidatorErrorPolicy};
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
        overwrite_artifacts: Some(true),
        artifacts: vec![artifact()],
    }])
    .await;

    exercise(&[StageParts {
        accepts: vec!["image/*".to_string()],
        as_text: vec!["text/plain".to_string()],
    }])
    .await;
}

/// `From<leviath_core::output::OutputSpec>` carries the artifacts and the
/// validator policy across, not just the top-level strings.
#[test]
fn the_conversion_carries_artifacts_and_the_validator_policy() {
    use leviath_core::output::{ArtifactSpec, OnValidatorError};

    let core = leviath_core::output::OutputSpec {
        format: Some("json".to_string()),
        on_validator_error: Some(OnValidatorError::Accept),
        artifacts: vec![ArtifactSpec {
            name: "report".to_string(),
            mime_type: "text/markdown".to_string(),
            required: true,
            description: None,
        }],
        ..leviath_core::output::OutputSpec::default()
    };
    let mapped = OutputSpec::from(&core);
    assert_eq!(mapped.format, Some("json".to_string()));
    assert_eq!(
        mapped.on_validator_error,
        Some(ValidatorErrorPolicy::Accept)
    );
    assert_eq!(mapped.artifacts.len(), 1);
    assert_eq!(mapped.artifacts[0].name, "report");
}
