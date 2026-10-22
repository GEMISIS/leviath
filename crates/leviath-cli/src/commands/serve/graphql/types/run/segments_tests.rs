//! `RunSegment`, read through a schema, and `Run.continuable`,
//! `Run.segments` and `Run.acceptsMessages` for a finished run.

use std::sync::Arc;

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};
use leviath_runtime::runfile::Segment;
use leviath_runtime::spec::names::{RunId, StageName};
use leviath_runtime::state::journal::ArtifactState;
use leviath_runtime::state::{FinalOutputState, RunStatus as State, Spend};

use super::super::Run;
use super::*;

fn segment(status: State, output: bool) -> Segment {
    Segment {
        index: 2,
        stage: StageName::new("revise").unwrap(),
        started_seq: 7,
        started_at: 100,
        forked_from: Some(RunId::new("src-1").unwrap()),
        status,
        spend: Spend {
            prompt_tokens: 12,
            completion_tokens: 3,
            priced_usd: 0.5,
            computed_calls: 1,
            ..Spend::default()
        },
        tool_calls: 4,
        final_output: output.then(|| FinalOutputState {
            bytes: 3,
            format: Some("markdown".into()),
            stage: StageName::new("revise").unwrap(),
            submitted_at: 120,
            truncated: false,
            artifacts: vec![ArtifactState {
                name: "cape.png".into(),
                path: "out/cape.png".into(),
                mime_type: "image/png".into(),
                size: 3,
                sha256: String::new(),
            }],
        }),
        file: None,
        last_seq: 9,
        current: true,
    }
}

struct Probe(Vec<SegmentRead>);

#[async_graphql::Object]
impl Probe {
    async fn segments(&self) -> Vec<RunSegment> {
        self.0.iter().cloned().map(RunSegment).collect()
    }
}

async fn ask(reads: Vec<SegmentRead>, query: &str) -> serde_json::Value {
    let schema = Schema::build(Probe(reads), EmptyMutation, EmptySubscription).finish();
    let answer = schema.execute(Request::new(query)).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    serde_json::to_value(&answer.data).unwrap()
}

#[tokio::test]
async fn a_segment_reads_its_own_spend_answer_and_files() {
    let reads = vec![
        SegmentRead {
            segment: segment(State::Complete, true),
            answer: Some("red".into()),
        },
        // An answer whose bytes did not read is no answer.
        SegmentRead {
            segment: segment(State::Error("x".into()), true),
            answer: None,
        },
        SegmentRead {
            segment: segment(State::Active, false),
            answer: None,
        },
    ];
    let json = ask(
        reads,
        "{ segments { index current stage startedAt startedSeq lastSeq status forkedFrom
           usage { promptTokens completionTokens cachedTokens cacheWriteTokens }
           cost { costUsd costPricedUsd costIsExact unpricedCalls }
           toolCallCount finalOutput { content format stage submittedAt truncated } artifacts } }",
    )
    .await;
    let first = &json["segments"][0];
    assert_eq!(first["index"], 2);
    assert_eq!(first["current"], true);
    assert_eq!(first["stage"], "revise");
    assert_eq!(first["startedSeq"], 7);
    assert_eq!(first["status"], "COMPLETE");
    assert_eq!(first["forkedFrom"], "src-1");
    assert_eq!(first["usage"]["promptTokens"], 12);
    assert_eq!(first["cost"]["costIsExact"], false);
    assert_eq!(first["toolCallCount"], 4);
    assert_eq!(first["finalOutput"]["content"], "red");
    assert_eq!(first["artifacts"][0], "cape.png");
    assert_eq!(json["segments"][1]["status"], "ERROR");
    assert!(json["segments"][1]["finalOutput"].is_null());
    assert!(json["segments"][2]["finalOutput"].is_null());
    assert_eq!(json["segments"][2]["status"], "RUNNING");
}

/// Every status a segment stands in reads as the run's own.
#[tokio::test]
async fn every_segment_status_has_a_run_status() {
    let reads = [State::Idle, State::Waiting, State::Paused, State::Cancelled]
        .into_iter()
        .map(|status| SegmentRead {
            segment: segment(status, false),
            answer: None,
        })
        .collect();
    let json = ask(reads, "{ segments { status } }").await;
    let words: Vec<&str> = json["segments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["status"].as_str().unwrap())
        .collect();
    assert_eq!(words, ["STARTING", "WAITING_INPUT", "PAUSED", "CANCELLED"]);
}

/// A finished run on disk says it can be continued, that a message reaches
/// it, and lists its one segment; a run with no file says neither.
#[tokio::test]
async fn a_finished_run_says_it_can_be_continued() {
    crate::runstate::with_isolated_runs_dir_async("graphql-continuable", |_d| async move {
        let mut meta = crate::test_fixtures::fixtures::run_meta("r-1");
        meta.status = leviath_core::run_meta::RunStatus::Complete;
        let query = "{ run { continuable acceptsMessages segments { index } } }";
        let json = ask_run(meta.clone(), query).await;
        assert!(json["run"]["continuable"].is_null());
        assert!(json["run"]["segments"].as_array().unwrap().is_empty());
        crate::runstate::create_run(&meta).unwrap();
        let json = ask_run(meta, query).await;
        assert_eq!(json["run"]["continuable"], true);
        assert_eq!(json["run"]["acceptsMessages"], true);
        assert_eq!(json["run"]["segments"][0]["index"], 1);
    })
    .await;
}

/// Ask a schema holding `meta`'s run for `query`.
async fn ask_run(meta: RunMeta, query: &str) -> serde_json::Value {
    struct One(Run);
    #[async_graphql::Object]
    impl One {
        async fn run(&self) -> &Run {
            &self.0
        }
    }
    let run = Run {
        meta: Arc::new(meta),
        now: 1,
    };
    let schema = Schema::build(One(run), EmptyMutation, EmptySubscription).finish();
    let answer = schema.execute(Request::new(query)).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    serde_json::to_value(&answer.data).unwrap()
}

/// A run file that will not read fails the segment list with an error rather
/// than answering an empty one.
#[tokio::test]
async fn a_run_file_that_will_not_read_fails_the_segment_list() {
    crate::runstate::with_isolated_runs_dir_async("graphql-segments-junk", |_d| async move {
        let mut meta = crate::test_fixtures::fixtures::run_meta("r-1");
        meta.status = leviath_core::run_meta::RunStatus::Complete;
        crate::runstate::create_run(&meta).unwrap();
        std::fs::write(crate::commands::serve::core::run_file::path("r-1"), b"junk").unwrap();
        assert!(super::read("r-1").await.is_err());
    })
    .await;
}
