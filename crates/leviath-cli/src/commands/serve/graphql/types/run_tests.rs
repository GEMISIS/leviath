//! Tests for the `Run` object and its mirror.

use std::sync::Arc;

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};

use super::{
    ContextSnapshotPoint, CostBreakdown, CurrentStage, MetadataEntry, Run, RunOrderField,
    RunStatus, TokenUsage, WorkingClock, as_i32,
};
use crate::commands::serve::graphql::filter::testkit::{
    exercise, exercise_enum, exercise_list, exercise_order,
};
use crate::commands::serve::graphql::query::Query;
use crate::commands::serve::graphql::scalars::{BigInt, Decimal, Timestamp};
use crate::commands::serve::graphql::types::run_detail::{ContextWindow, StageModelUse};
use crate::runstate::RunMeta;

/// A run with every field this module reads set to something distinguishable.
fn meta() -> RunMeta {
    let mut meta = RunMeta::new(
        "coder-1788924523-abc123".to_string(),
        "coder".to_string(),
        "/agents/coder/agent.leviath".to_string(),
        "fix the parser".to_string(),
        Some("gpt-5.6".to_string()),
        "/work".to_string(),
        1,
    );
    meta.started_at = 1_788_924_523;
    meta.updated_at = 1_788_924_600;
    meta.prompt_tokens = 1_200;
    meta.completion_tokens = 300;
    meta.cached_tokens = 900;
    meta.cache_write_tokens = 100;
    meta.cost_usd = Some(0.0425);
    meta.cost_priced_usd = 0.0425;
    meta.cost_is_exact = true;
    meta.metadata.insert("ticket".to_string(), "42".to_string());
    meta.metadata
        .insert("author".to_string(), "ana".to_string());
    meta
}

/// Ask the schema for one run's fields, reading from the given record.
async fn ask(meta: RunMeta, query: &str) -> serde_json::Value {
    // The object under test is built directly rather than through a listing,
    // so this exercises the field resolvers without a runs directory.
    let run = Run {
        meta: Arc::new(meta),
        now: 1_788_925_000,
    };
    let schema = Schema::build(RunProbe { run }, EmptyMutation, EmptySubscription).finish();
    let answer = schema.execute(Request::new(query)).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    serde_json::to_value(&answer.data).expect("data serializes")
}

/// A root that hands out exactly one run, so a field test is one query.
struct RunProbe {
    run: Run,
}

#[async_graphql::Object]
impl RunProbe {
    /// The run under test.
    async fn run(&self) -> &Run {
        &self.run
    }
}

/// Every daemon state has exactly one schema value, and the conversion is
/// exhaustive.
#[test]
fn every_daemon_status_maps_to_one_schema_status() {
    use leviath_core::run_meta::RunStatus as Daemon;
    let pairs = [
        (Daemon::Starting, RunStatus::Starting),
        (Daemon::Running, RunStatus::Running),
        (Daemon::WaitingInput, RunStatus::WaitingInput),
        (Daemon::Paused, RunStatus::Paused),
        (Daemon::Complete, RunStatus::Complete),
        (Daemon::CompleteInteractive, RunStatus::CompleteInteractive),
        (Daemon::Error, RunStatus::Error),
        (Daemon::Cancelled, RunStatus::Cancelled),
    ];
    for (daemon, schema) in pairs {
        assert_eq!(RunStatus::from(&daemon), schema, "{daemon:?}");
    }
}

/// A counter past 32 bits reads as an implausible ceiling rather than wrapping
/// to a small number that looks reasonable.
#[test]
fn a_counter_that_cannot_fit_saturates() {
    assert_eq!(as_i32(7), 7);
    assert_eq!(as_i32(usize::MAX), i32::MAX);
}

/// The plain fields, read as a client reads them.
#[tokio::test]
async fn a_run_carries_its_identity_counters_and_spend() {
    let json = ask(
        meta(),
        r#"{ run { id blueprintName task status startedAt updatedAt ageSecs workingSecs
                   usage { promptTokens completionTokens cachedTokens cacheWriteTokens }
                   cost { costUsd costPricedUsd costIsExact unpricedCalls }
                   workdir } }"#,
    )
    .await;
    let run = &json["run"];
    assert_eq!(run["id"], "coder-1788924523-abc123");
    assert_eq!(run["blueprintName"], "coder");
    assert_eq!(run["task"], "fix the parser");
    assert_eq!(run["status"], "STARTING");
    assert_eq!(run["startedAt"], 1_788_924_523i64);
    assert_eq!(run["ageSecs"], 477);
    assert_eq!(run["usage"]["promptTokens"], 1_200);
    assert_eq!(run["usage"]["cachedTokens"], 900);
    // Money crosses the wire as a string, so no JSON parser re-rounds it.
    assert_eq!(run["cost"]["costUsd"], "0.0425");
    assert_eq!(run["cost"]["costIsExact"], true);
    assert_eq!(run["cost"]["unpricedCalls"], 0);
    assert_eq!(run["workdir"], "/work");
}

/// Absent is absent: a run with no title, no error and no parent says so with
/// nulls rather than empty strings, and its ancestry is empty.
#[tokio::test]
async fn what_a_run_does_not_have_reads_as_null() {
    let json = ask(
        meta(),
        "{ run { title titleError error lastProgressAt parentId active { bankedSecs }
                 stageModels { provider model } } }",
    )
    .await;
    let run = &json["run"];
    assert!(run["title"].is_null());
    assert!(run["titleError"].is_null());
    assert!(run["error"].is_null());
    assert!(run["lastProgressAt"].is_null());
    assert!(run["parentId"].is_null());
    assert!(run["active"].is_null());
    assert_eq!(run["stageModels"].as_array().map(Vec::len), Some(0));
}

/// The states that own a field carry it: an errored run has its message, a
/// titled run its title, a sub-agent its parent.
#[tokio::test]
async fn the_fields_a_state_owns_are_set() {
    let mut meta = meta();
    meta.status = leviath_core::run_meta::RunStatus::Error;
    meta.error = Some("provider refused".to_string());
    meta.title = Some("Parser fix".to_string());
    meta.title_error = Some("titling model unavailable".to_string());
    meta.last_progress_at = Some(1_788_924_590);
    meta.parent_run_id = Some("root-1788924000-aaa111".to_string());
    meta.yolo = true;
    meta.yolo_profile = Some("solo".to_string());
    meta.active = Some(leviath_core::run_meta::ActiveClock {
        banked_secs: 40,
        since: Some(1_788_924_900),
    });
    meta.stage_models = vec![leviath_core::run_meta::StageModelUse {
        provider: "anthropic".to_string(),
        model: "claude".to_string(),
    }];

    let json = ask(
        meta,
        r#"{ run { status error title titleError lastProgressAt parentId unattended
                   yoloProfileName active { bankedSecs since } workingSecs
                   stageModels { provider model } } }"#,
    )
    .await;
    let run = &json["run"];
    assert_eq!(run["status"], "ERROR");
    assert_eq!(run["error"], "provider refused");
    assert_eq!(run["title"], "Parser fix");
    assert_eq!(run["titleError"], "titling model unavailable");
    assert_eq!(run["lastProgressAt"], 1_788_924_590i64);
    assert_eq!(run["parentId"], "root-1788924000-aaa111");
    assert_eq!(run["unattended"], true);
    assert_eq!(run["yoloProfileName"], "solo");
    assert_eq!(run["active"]["bankedSecs"], 40);
    assert_eq!(run["active"]["since"], 1_788_924_900i64);
    // Banked plus the span in progress, not wall-clock age.
    assert_eq!(run["workingSecs"], 140);
    assert_eq!(run["stageModels"][0]["provider"], "anthropic");
    assert_eq!(run["stageModels"][0]["model"], "claude");
}

/// Metadata is stored in a hash map, so the listing is sorted: two identical
/// requests that answer in different orders are a diff nobody can read.
#[tokio::test]
async fn metadata_comes_back_in_a_stable_order() {
    let json = ask(meta(), "{ run { metadata { key value } } }").await;
    let entries = json["run"]["metadata"].as_array().expect("entries");
    let keys: Vec<&str> = entries
        .iter()
        .map(|e| e["key"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(keys, vec!["author", "ticket"]);
    assert_eq!(entries[1]["value"], "42");
}

/// The counters a run keeps as it works.
#[tokio::test]
async fn the_progress_counters_are_carried() {
    let mut meta = meta();
    meta.iteration = 3;
    meta.tool_calls = 17;
    let json = ask(meta, "{ run { iteration toolCallCount } }").await;
    assert_eq!(json["run"]["iteration"], 3);
    assert_eq!(json["run"]["toolCallCount"], 17);
}

/// The stage a run is in, by name and position, and nothing before it enters
/// one.
#[tokio::test]
async fn the_current_stage_is_null_until_the_run_enters_one() {
    let json = ask(meta(), "{ run { currentStage { name index of } } }").await;
    assert!(json["run"]["currentStage"].is_null());

    let mut entered = meta();
    entered.current_stage = "build".to_string();
    entered.stage_index = 1;
    entered.num_stages = 3;
    let json = ask(entered, "{ run { currentStage { name index of } } }").await;
    assert_eq!(json["run"]["currentStage"]["name"], "build");
    assert_eq!(json["run"]["currentStage"]["index"], 1);
    assert_eq!(json["run"]["currentStage"]["of"], 3);
}

/// The `Query` root is what the server actually serves, so it is asked for a
/// field here too: a type that compiles but does not register would otherwise
/// pass every test above.
#[test]
fn the_served_schema_exposes_the_run_listing() {
    let sdl = Schema::build(Query, EmptyMutation, EmptySubscription)
        .finish()
        .sdl();
    assert!(sdl.contains("type RunOutput "), "{sdl}");
    assert!(sdl.contains("runs("), "{sdl}");
    assert!(sdl.contains("input RunInput "), "{sdl}");
    assert!(sdl.contains("enum RunOrderField "), "{sdl}");
}

/// Every function the mirror wrote for one of this file's types runs once.
#[tokio::test]
async fn every_mirrored_function_runs() {
    let run = Run {
        meta: Arc::new(meta()),
        now: 1_788_925_000,
    };
    exercise(std::slice::from_ref(&run)).await;
    exercise_order(&run, RunOrderField::ALL);
    exercise_enum(&[RunStatus::Starting]).await;
    exercise(&[TokenUsage {
        prompt_tokens: BigInt(1),
        completion_tokens: BigInt(2),
        cached_tokens: BigInt(3),
        cache_write_tokens: BigInt(4),
    }])
    .await;
    exercise(&[CostBreakdown {
        cost_usd: Some(Decimal(1.0)),
        cost_priced_usd: Decimal(1.0),
        cost_is_exact: true,
        unpriced_calls: 0,
    }])
    .await;
    exercise(&[WorkingClock {
        banked_secs: 1,
        since: Some(Timestamp(2)),
    }])
    .await;
    exercise(&[CurrentStage {
        name: "build".to_string(),
        index: 0,
        of: 1,
    }])
    .await;
    exercise(&[ContextSnapshotPoint {
        at: Timestamp(1),
        stage: "build".to_string(),
        window: ContextWindow {
            snapshot: std::sync::Arc::new(leviath_core::run_meta::ContextSnapshot {
                stage_name: "build".to_string(),
                total_tokens: 0,
                max_tokens: 100,
                regions: Vec::new(),
            }),
        },
    }])
    .await;
    let entries = [MetadataEntry {
        key: "k".to_string(),
        value: "v".to_string(),
    }];
    exercise(&entries).await;
    exercise_list(&entries).await;
    // The run's own list of what its stages ran on is read through the mirror
    // that `run_detail` wrote for it.
    exercise_list(&[StageModelUse {
        provider: "anthropic".to_string(),
        model: "claude".to_string(),
    }])
    .await;
}

/// The `logs` argument bags read back from their own value, and refuse a field
/// of the wrong type.
#[test]
fn the_log_options_round_trip() {
    use super::super::super::filter::testkit::round_trip;
    use super::support::LogStageOptions;

    round_trip(&LogStageOptions::Index(0));
    round_trip(&LogStageOptions::All(true));
}
