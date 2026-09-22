//! Tests for the run-behaviour settings, and for the mirrors of those types.

use std::sync::Arc;

use super::{
    BlueprintSecurity, CompactionConfig, FileTrackingConfig, NudgeConfig, NudgePolicy,
    RepetitionDetection, SafeCommands, SandboxConfig, SandboxKind, SandboxUnavailable, StageHooks,
    TaintTracking, WorkerFailurePolicy,
};
use crate::commands::serve::graphql::filter::testkit::{exercise, exercise_enum};

/// A `FileTrackingConfig` object, built the way `Blueprint.fileTracking`
/// builds one: from a parsed manifest that declares the block.
fn file_tracking_config() -> FileTrackingConfig {
    let text = "[agent]\n\
                name = \"t\"\n\
                version = \"1.0.0\"\n\
                description = \"d\"\n\
                \n\
                [context.regions.files]\n\
                kind = \"hashmap\"\n\
                max_tokens = 800\n\
                \n\
                [context.file_tracking]\n\
                region = \"files\"\n\
                track_reads = true\n\
                track_writes = false\n";
    let parsed = leviath_core::manifest::parse_manifest(text).expect("the manifest parses");
    let blueprint = Arc::new(parsed);
    let tracking = blueprint
        .file_tracking
        .clone()
        .expect("the manifest declares file tracking");
    FileTrackingConfig::of(&blueprint, &tracking)
}

/// Every function `#[mirror]` wrote for this file's types runs at least once.
#[tokio::test]
async fn every_mirrored_function_runs() {
    exercise_enum(&[
        NudgePolicy::Inherit,
        NudgePolicy::Nudge,
        NudgePolicy::NeverNudge,
    ])
    .await;
    exercise(&[NudgeConfig {
        policy: NudgePolicy::Nudge,
        max: Some(3),
        text: Some("keep going".to_string()),
    }])
    .await;

    exercise_enum(&[TaintTracking::Inherit, TaintTracking::Track]).await;
    exercise(&[BlueprintSecurity {
        taint_tracking: TaintTracking::Track,
    }])
    .await;

    exercise_enum(&[
        SandboxKind::None,
        SandboxKind::Namespace,
        SandboxKind::Container,
    ])
    .await;
    exercise_enum(&[SandboxUnavailable::Error, SandboxUnavailable::Warn]).await;
    exercise(&[SandboxConfig {
        kind: SandboxKind::Container,
        image: Some("python:3.12".to_string()),
        engine: None,
        allow_network: false,
        mounts: vec!["./data:/data".to_string()],
        keep_warm: true,
        on_unavailable: SandboxUnavailable::Error,
    }])
    .await;

    exercise(&[StageHooks {
        on_stage_enter: Some("hooks/enter.rhai".to_string()),
        on_stage_exit: None,
        before_inference: None,
        after_inference: None,
        on_tool_call: None,
        on_completion: None,
        on_error: None,
    }])
    .await;

    exercise(&[SafeCommands {
        tools: vec!["read_file".to_string()],
        shell: vec!["cargo test".to_string()],
    }])
    .await;

    exercise(&[RepetitionDetection {
        enabled: Some(true),
        max_repeat_calls: Some(4),
        max_readonly_streak: None,
    }])
    .await;

    let tracking = file_tracking_config();
    exercise(std::slice::from_ref(&tracking)).await;

    exercise(&[CompactionConfig {
        provider: "anthropic".to_string(),
        model: "claude-haiku-4-5".to_string(),
        system_prompt: None,
        user_prompt_template: None,
        max_summary_tokens: 800,
        temperature: 0.1,
    }])
    .await;

    exercise_enum(&[WorkerFailurePolicy::Continue, WorkerFailurePolicy::FailAll]).await;
}

/// The nudge policy's three states read back from what a manifest can write:
/// nothing, `true` or `false`.
#[test]
fn nudge_policy_reads_the_three_states_a_manifest_can_write() {
    assert_eq!(NudgePolicy::from(None), NudgePolicy::Inherit);
    assert_eq!(NudgePolicy::from(Some(true)), NudgePolicy::Nudge);
    assert_eq!(NudgePolicy::from(Some(false)), NudgePolicy::NeverNudge);
}

/// `FileTrackingConfig` carries the block's own fields, read through the
/// object rather than the struct.
#[tokio::test]
async fn file_tracking_carries_its_block() {
    use async_graphql::{EmptyMutation, EmptySubscription, Object, Request, Schema};

    struct Probe {
        tracking: FileTrackingConfig,
    }

    #[Object]
    impl Probe {
        async fn tracking(&self) -> &FileTrackingConfig {
            &self.tracking
        }
    }

    let schema = Schema::build(
        Probe {
            tracking: file_tracking_config(),
        },
        EmptyMutation,
        EmptySubscription,
    )
    .finish();
    let answer = schema
        .execute(Request::new(
            "{ tracking { region { name } regionName trackReads trackWrites maxFileTokens } }",
        ))
        .await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    let json = serde_json::to_value(&answer.data).expect("data serializes");
    assert_eq!(json["tracking"]["region"]["name"], "files");
    assert_eq!(json["tracking"]["regionName"], "files");
    assert_eq!(json["tracking"]["trackReads"], true);
    assert_eq!(json["tracking"]["trackWrites"], false);
    assert!(json["tracking"]["maxFileTokens"].is_null());
}
