//! Tests for what a run's provider calls took: the inferences field, the
//! outcome typing behind it, and the move that follows a call nobody could
//! serve.
//!
//! These write a real journal and read it back through the schema, the same way
//! `interaction_tests.rs` does, because the whole surface is a reading of that
//! file.

use std::sync::Arc;

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};
use leviath_core::run_archive::{
    self, AttemptOutcome, AttemptRecord, FailoverRecord, RequestDigest, Retry, RunIdentity,
    RunRecord,
};

use super::super::run::Run;
use crate::commands::serve::testutil::state_with_agent_paths;
use crate::runstate::{RunMeta, create_run};

/// A run to hang a journal off.
fn meta() -> RunMeta {
    let mut meta = RunMeta::new(
        "called-providers".to_string(),
        "coder".to_string(),
        "/agents/coder/agent.leviath".to_string(),
        "call a provider a few times".to_string(),
        None,
        "/tmp".to_string(),
        1,
    );
    meta.started_at = 1_788_924_523;
    meta.updated_at = 1_788_924_600;
    meta
}

/// A root handing out one run.
struct Probe {
    run: Run,
}

#[async_graphql::Object]
impl Probe {
    /// The run under test.
    async fn run(&self) -> &Run {
        &self.run
    }
}

/// The data of a query that is expected to work.
async fn data(query: &str) -> serde_json::Value {
    let run = Run {
        meta: Arc::new(meta()),
        now: 1_788_925_000,
    };
    let schema = Schema::build(Probe { run }, EmptyMutation, EmptySubscription)
        .data(state_with_agent_paths(Vec::new()))
        .finish();
    let answer = schema.execute(Request::new(query)).await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    serde_json::to_value(&answer.data).expect("data serializes")
}

/// The first error of a query that is expected to fail.
async fn error(query: &str) -> String {
    let run = Run {
        meta: Arc::new(meta()),
        now: 1_788_925_000,
    };
    let schema = Schema::build(Probe { run }, EmptyMutation, EmptySubscription)
        .data(state_with_agent_paths(Vec::new()))
        .finish();
    let answer = schema.execute(Request::new(query)).await;
    answer
        .errors
        .first()
        .map(|e| e.message.clone())
        .expect("a refusal")
}

/// One attempt record.
fn attempt(n: u32, provider: &str, model: &str, outcome: AttemptOutcome) -> RunRecord {
    RunRecord::InferenceAttempt(AttemptRecord {
        id: format!("a{n:08x}"),
        stage: "plan".to_string(),
        attempt: n,
        provider: provider.to_string(),
        model: model.to_string(),
        outcome,
        duration_ms: 1_200,
        backoff_ms: 400,
        digest: RequestDigest {
            system_hash: 0x1234_5678_9abc_def0,
            messages: 4,
            tools: 7,
            max_tokens: 2048,
            temperature: 0.25,
        },
        model_input: None,
        at: 200,
    })
}

/// One attempt whose journal recorded a model input in `status`.
fn attempt_with_input(status: run_archive::CaptureStatus) -> RunRecord {
    let retained = status == run_archive::CaptureStatus::Retained;
    let body = serde_json::json!({ "model": "claude-sonnet-4-5", "messages": [] });
    let RunRecord::InferenceAttempt(mut record) =
        attempt(1, "anthropic", "claude", AttemptOutcome::Succeeded)
    else {
        unreachable!("attempt builds an attempt record")
    };
    record.model_input = Some(run_archive::ModelInput {
        capture_status: status,
        request: retained.then(|| body.clone()),
        bytes: body.to_string().len() as u64,
        source_context_digest: "0f0f0f0f0f0f0f0f".to_string(),
        parameters: [
            ("temperature".to_string(), serde_json::json!(0.25)),
            ("max_output_tokens".to_string(), serde_json::json!(2048)),
            ("top_p".to_string(), serde_json::json!(0.9)),
        ]
        .into_iter()
        .collect(),
        tool_catalog_version: "fedcba9876543210".to_string(),
        assembly_version: "1".to_string(),
    });
    RunRecord::InferenceAttempt(record)
}

/// One failover record, with whatever classification the error carried.
fn failover(kind: &str) -> RunRecord {
    RunRecord::InferenceFailover(FailoverRecord {
        stage: "plan".to_string(),
        iteration: 3,
        from_provider: "anthropic".to_string(),
        from_model: "claude-sonnet-4-5".to_string(),
        to_provider: "openai".to_string(),
        to_model: "gpt-5".to_string(),
        reason: "credits_exhausted".to_string(),
        kind: kind.to_string(),
        at: 201,
    })
}

/// Write a journal of `records` for the run.
fn write_journal(records: Vec<RunRecord>) {
    let meta = meta();
    let mut buf = Vec::new();
    run_archive::write_archive_start(&mut buf, run_archive::RUN_ARCHIVE_VERSION)
        .expect("a preamble");
    run_archive::write_record(
        &mut buf,
        &RunRecord::Header {
            identity: RunIdentity {
                run_id: meta.run_id.clone(),
                machine_id: "m".to_string(),
                world_id: "w".to_string(),
                created_at: 0,
            },
            meta: Box::new(meta.clone()),
        },
    )
    .expect("a header");
    for record in &records {
        run_archive::write_record(&mut buf, record).expect("a record");
    }
    std::fs::write(
        crate::runstate::run_dir(&meta.run_id).join(leviath_core::files::ARCHIVE_FILE),
        &buf,
    )
    .expect("the journal");
}

/// Every field reads back typed: the failure detail is present only where the
/// attempt failed, and the move to another provider hangs off the attempt it
/// followed.
#[tokio::test]
async fn the_attempts_read_back_typed_with_their_outcome() {
    crate::runstate::with_isolated_runs_dir_async("graphql-inferences", |_dir| async move {
        create_run(&meta()).expect("run written");
        write_journal(vec![
            attempt(
                1,
                "anthropic",
                "claude-sonnet-4-5",
                AttemptOutcome::Failed {
                    kind: "insufficient_credits".to_string(),
                    transient: false,
                    capacity: false,
                    next: Retry::Reported,
                },
            ),
            failover("insufficient_credits"),
            attempt(1, "openai", "gpt-5", AttemptOutcome::Succeeded),
        ]);

        let json = data(
            r#"{ run { inferences(first: 10) {
                 total cursor
                 results {
                   stage attempt provider model durationMs backoffMs at
                   outcome { kind failureKind transient capacity retry }
                   digest { systemHash messages tools maxTokens temperature }
                   failover {
                     stage iteration fromProvider fromModel toProvider toModel
                     reason failureKind at
                   }
                 }
               } } }"#,
        )
        .await;
        let page = &json["run"]["inferences"];
        assert_eq!(page["total"], 2, "a move is not an attempt of its own");
        assert!(page["cursor"].is_null(), "the only page");

        let first = &page["results"][0];
        assert_eq!(first["stage"], "plan");
        assert_eq!(first["attempt"], 1);
        assert_eq!(first["provider"], "anthropic");
        assert_eq!(first["model"], "claude-sonnet-4-5");
        assert_eq!(first["durationMs"], 1_200);
        assert_eq!(first["backoffMs"], 400);
        assert_eq!(first["at"], 200);
        assert_eq!(first["outcome"]["kind"], "FAILED");
        assert_eq!(first["outcome"]["failureKind"], "insufficient_credits");
        assert_eq!(first["outcome"]["transient"], false);
        assert_eq!(first["outcome"]["capacity"], false);
        assert_eq!(first["outcome"]["retry"], "REPORTED");
        // The hash goes out as fixed-width hex: a u64 is not an Int, and a
        // client compares it rather than reading anything into it.
        assert_eq!(first["digest"]["systemHash"], "123456789abcdef0");
        assert_eq!(first["digest"]["messages"], 4);
        assert_eq!(first["digest"]["tools"], 7);
        assert_eq!(first["digest"]["maxTokens"], 2048);
        assert_eq!(first["digest"]["temperature"], 0.25);
        let moved = &first["failover"];
        assert_eq!(moved["stage"], "plan");
        assert_eq!(moved["iteration"], 3);
        assert_eq!(moved["fromProvider"], "anthropic");
        assert_eq!(moved["fromModel"], "claude-sonnet-4-5");
        assert_eq!(moved["toProvider"], "openai");
        assert_eq!(moved["toModel"], "gpt-5");
        assert_eq!(moved["reason"], "credits_exhausted");
        assert_eq!(moved["failureKind"], "insufficient_credits");
        assert_eq!(moved["at"], 201);

        // The call that worked has no failure to classify, and nothing
        // followed it.
        let second = &page["results"][1];
        assert_eq!(second["provider"], "openai");
        assert_eq!(second["outcome"]["kind"], "SUCCEEDED");
        assert!(second["outcome"]["failureKind"].is_null());
        assert!(second["outcome"]["transient"].is_null());
        assert!(second["outcome"]["capacity"].is_null());
        assert!(second["outcome"]["retry"].is_null());
        assert!(second["failover"].is_null());
    })
    .await;
}

/// A failure the provider gave no classification for reads as null rather than
/// as an empty label, on the attempt and on the move alike.
#[tokio::test]
async fn an_unclassified_failure_has_no_kind() {
    crate::runstate::with_isolated_runs_dir_async(
        "graphql-inferences-unclassified",
        |_dir| async move {
            create_run(&meta()).expect("run written");
            write_journal(vec![
                attempt(
                    1,
                    "anthropic",
                    "claude-sonnet-4-5",
                    AttemptOutcome::Failed {
                        kind: String::new(),
                        transient: true,
                        capacity: true,
                        next: Retry::SameModel,
                    },
                ),
                failover(""),
            ]);

            let json = data(
                "{ run { inferences(first: 10) { results { \
                 outcome { kind failureKind transient capacity retry } \
                 failover { failureKind reason } } } } }",
            )
            .await;
            let node = &json["run"]["inferences"]["results"][0];
            assert_eq!(node["outcome"]["kind"], "FAILED");
            assert!(node["outcome"]["failureKind"].is_null());
            assert_eq!(node["outcome"]["transient"], true);
            assert_eq!(node["outcome"]["capacity"], true);
            assert_eq!(node["outcome"]["retry"], "SAME_MODEL");
            assert!(node["failover"]["failureKind"].is_null());
            assert_eq!(node["failover"]["reason"], "credits_exhausted");
        },
    )
    .await;
}

/// The page carries on from its cursor, and the last page says it is the last.
#[tokio::test]
async fn the_attempts_page_carries_on_from_its_cursor() {
    crate::runstate::with_isolated_runs_dir_async("graphql-inferences-paging", |_dir| async move {
        create_run(&meta()).expect("run written");
        write_journal(
            (1..=5)
                .map(|i| {
                    attempt(
                        i,
                        "openai",
                        "gpt-5",
                        AttemptOutcome::Failed {
                            kind: "rate_limited".to_string(),
                            transient: true,
                            capacity: true,
                            next: Retry::RenewedFiles,
                        },
                    )
                })
                .collect(),
        );

        let json = data(
            "{ run { inferences(first: 2) { total cursor \
             results { attempt outcome { retry } } } } }",
        )
        .await;
        let page = &json["run"]["inferences"];
        assert_eq!(page["total"], 5);
        assert!(page["cursor"].as_str().is_some(), "more to come");
        assert_eq!(page["results"][0]["attempt"], 1);
        assert_eq!(page["results"][0]["outcome"]["retry"], "RENEWED_FILES");
        let cursor = page["cursor"].as_str().expect("a cursor");

        let json = data(&format!(
            r#"{{ run {{ inferences(first: 10, after: "{cursor}") {{
                 cursor results {{ attempt }}
               }} }} }}"#
        ))
        .await;
        let page = &json["run"]["inferences"];
        assert!(page["cursor"].is_null(), "that was the rest");
        let numbers: Vec<i64> = page["results"]
            .as_array()
            .expect("results")
            .iter()
            .filter_map(|node| node["attempt"].as_i64())
            .collect();
        assert_eq!(numbers, vec![3, 4, 5], "no attempt read twice");
    })
    .await;
}

/// A page larger than the cap is refused, saying what the cap is.
#[tokio::test]
async fn a_page_over_the_cap_is_refused() {
    crate::runstate::with_isolated_runs_dir_async("graphql-inferences-cap", |_dir| async move {
        create_run(&meta()).expect("run written");
        let message = error("{ run { inferences(first: 5000) { total } } }").await;
        assert!(message.contains("at most 200"), "{message}");
        let message = error("{ run { inferences(first: 0) { total } } }").await;
        assert!(message.contains("at least 1"), "{message}");
    })
    .await;
}

/// A cursor from another listing is refused rather than resumed somewhere else.
#[tokio::test]
async fn a_cursor_from_elsewhere_is_refused() {
    crate::runstate::with_isolated_runs_dir_async("graphql-inferences-cursor", |_dir| async move {
        create_run(&meta()).expect("run written");
        let message =
            error(r#"{ run { inferences(first: 2, after: "not-a-cursor-from-here") { total } } }"#)
                .await;
        assert!(!message.is_empty(), "it says why");
    })
    .await;
}

/// Naming `orderBy` explicitly reverses the order the attempts were made in.
#[tokio::test]
async fn an_explicit_order_by_walks_backwards() {
    crate::runstate::with_isolated_runs_dir_async(
        "graphql-inferences-orderby",
        |_dir| async move {
            create_run(&meta()).expect("run written");
            write_journal(vec![
                attempt(1, "anthropic", "claude", AttemptOutcome::Succeeded),
                attempt(2, "openai", "gpt-5", AttemptOutcome::Succeeded),
            ]);

            let json = data(
                "{ run { inferences(orderBy: [{ field: SEQUENCE, direction: DESC }]) { \
             results { attempt } } } }",
            )
            .await;
            let attempts: Vec<i64> = json["run"]["inferences"]["results"]
                .as_array()
                .expect("results")
                .iter()
                .filter_map(|node| node["attempt"].as_i64())
                .collect();
            assert_eq!(attempts, vec![2, 1], "descending reverses the order made");
        },
    )
    .await;
}

/// A filter nested past the depth limit is refused rather than walked.
#[tokio::test]
async fn a_filter_past_the_depth_limit_is_refused() {
    crate::runstate::with_isolated_runs_dir_async(
        "graphql-inferences-filter-depth",
        |_dir| async move {
            create_run(&meta()).expect("run written");
            let mut filter = r#"{ provider: { eq: "x" } }"#.to_string();
            for _ in 0..20 {
                filter = format!("{{ not: {filter} }}");
            }
            let message = error(&format!(
                "{{ run {{ inferences(filter: {filter}) {{ total }} }} }}"
            ))
            .await;
            assert!(message.contains("flatten it"), "{message}");
        },
    )
    .await;
}

/// A run that never called a provider has no attempts, and says so with an
/// empty page rather than an error.
#[tokio::test]
async fn a_run_that_never_called_a_provider_has_no_attempts() {
    crate::runstate::with_isolated_runs_dir_async("graphql-inferences-empty", |_dir| async move {
        create_run(&meta()).expect("run written");
        let json = data("{ run { inferences(first: 10) { total results { provider } } } }").await;
        assert_eq!(json["run"]["inferences"]["total"], 0);
        assert_eq!(
            json["run"]["inferences"]["results"]
                .as_array()
                .map(Vec::len),
            Some(0)
        );
    })
    .await;
}

/// A journal that cannot be read is an error about the journal, not an empty
/// list that looks like a run which never called anything.
#[tokio::test]
async fn an_unreadable_journal_says_so() {
    crate::runstate::with_isolated_runs_dir_async(
        "graphql-inferences-corrupt",
        |_dir| async move {
            create_run(&meta()).expect("run written");
            std::fs::write(
                crate::runstate::run_dir("called-providers")
                    .join(leviath_core::files::ARCHIVE_FILE),
                b"not an archive",
            )
            .expect("a corrupt journal");
            let message = error("{ run { inferences(first: 10) { total } } }").await;
            assert!(message.contains("unreadable journal"), "{message}");
        },
    )
    .await;
}

/// Every retry decision maps to its own word, with no fallback swallowing one.
///
/// All three are exercised end to end above; this pins the mapping itself,
/// which is the whole of what there is to get wrong.
#[test]
fn every_retry_decision_has_a_word() {
    use super::RetryDecision as Served;
    let cases = [
        (Retry::Reported, Served::Reported),
        (Retry::SameModel, Served::SameModel),
        (Retry::RenewedFiles, Served::RenewedFiles),
    ];
    for (core, served) in cases {
        assert_eq!(Served::from(core), served, "{core:?}");
    }
}

/// Every capture state maps to its own word, with no fallback swallowing one.
///
/// Only two are ever written today - a run is captured or it is not - and the
/// other two describe a record whose body was removed after the fact, which is a
/// state a reader has to be able to tell from "never taken".
#[test]
fn every_capture_state_has_a_word() {
    use super::CaptureStatus as Served;
    let cases = [
        (run_archive::CaptureStatus::Retained, Served::Retained),
        (run_archive::CaptureStatus::NotCaptured, Served::NotCaptured),
        (run_archive::CaptureStatus::Redacted, Served::Redacted),
        (run_archive::CaptureStatus::Expired, Served::Expired),
    ];
    for (core, served) in cases {
        assert_eq!(Served::from(core), served, "{core:?}");
    }
}

/// A captured attempt hands back the request itself, beside the window it was
/// assembled from and the parameters it really carried.
#[tokio::test]
async fn a_captured_attempt_serves_the_request_it_sent() {
    crate::runstate::with_isolated_runs_dir_async("graphql-model-input", |_dir| async move {
        create_run(&meta()).expect("run written");
        write_journal(vec![attempt_with_input(
            run_archive::CaptureStatus::Retained,
        )]);

        let json = data(
            r#"{ run { inferences(first: 10) { results { modelInput {
                 captureStatus request bytes sourceContextDigest
                 toolCatalogVersion assemblyVersion
                 parameters {
                   temperature
                   maxOutputTokens { __typename ... on MaxTokensCountOutput { tokens } }
                   providerParams
                 }
               } } } } }"#,
        )
        .await;
        let input = &json["run"]["inferences"]["results"][0]["modelInput"];
        assert_eq!(input["captureStatus"], "RETAINED");
        assert_eq!(input["request"]["model"], "claude-sonnet-4-5");
        assert_eq!(input["bytes"], 43);
        assert_eq!(input["sourceContextDigest"], "0f0f0f0f0f0f0f0f");
        assert_eq!(input["toolCatalogVersion"], "fedcba9876543210");
        assert_eq!(input["assemblyVersion"], "1");
        // The blueprint reader's own parameter shape, over the effective values:
        // the completion budget is an absolute count because that is what went
        // out, and a pass-through key stays where a declared one would.
        let parameters = &input["parameters"];
        assert_eq!(parameters["temperature"], 0.25);
        assert_eq!(
            parameters["maxOutputTokens"]["__typename"],
            "MaxTokensCountOutput"
        );
        assert_eq!(parameters["maxOutputTokens"]["tokens"], 2048);
        assert_eq!(parameters["providerParams"]["top_p"], 0.9);
    })
    .await;
}

/// An uncaptured attempt says so, and still answers everything capture does not
/// pay for.
#[tokio::test]
async fn an_uncaptured_attempt_carries_no_body_and_no_window_fingerprint() {
    crate::runstate::with_isolated_runs_dir_async("graphql-no-capture", |_dir| async move {
        create_run(&meta()).expect("run written");
        write_journal(vec![
            attempt_with_input(run_archive::CaptureStatus::NotCaptured),
            attempt(2, "openai", "gpt-5", AttemptOutcome::Succeeded),
        ]);

        let json = data(
            r#"{ run { inferences(first: 10) { results { modelInput {
                 captureStatus request toolCatalogVersion
               } } } } }"#,
        )
        .await;
        let results = &json["run"]["inferences"]["results"];
        let input = &results[0]["modelInput"];
        assert_eq!(input["captureStatus"], "NOT_CAPTURED");
        assert!(input["request"].is_null(), "{input}");
        assert_eq!(input["toolCatalogVersion"], "fedcba9876543210");
        // And an attempt whose journal recorded no model input at all is null
        // rather than an invented uncaptured one.
        assert!(results[1]["modelInput"].is_null(), "{results}");
    })
    .await;
}

/// Every function `#[mirror]` wrote for this file's types runs at least once.
///
/// The mirrors are straight lines of delegation, so running each of them once
/// is enough to measure all of them. One test per file rather than per query:
/// what a query happens to select is not what the mirror is made of.
#[tokio::test]
async fn every_mirrored_function_runs() {
    use crate::commands::serve::graphql::filter::testkit::{exercise, exercise_enum};

    exercise_enum(&[
        super::RetryDecision::Reported,
        super::RetryDecision::RenewedFiles,
    ])
    .await;
    exercise_enum(&[
        super::AttemptOutcomeKind::Succeeded,
        super::AttemptOutcomeKind::Failed,
    ])
    .await;
    exercise_enum(&[
        super::CaptureStatus::Retained,
        super::CaptureStatus::Expired,
    ])
    .await;

    exercise(&[
        super::AttemptOutcome::from(&AttemptOutcome::Succeeded),
        super::AttemptOutcome::from(&AttemptOutcome::Failed {
            kind: "insufficient_credits".to_string(),
            transient: true,
            capacity: false,
            next: Retry::Reported,
        }),
    ])
    .await;

    exercise(&[super::RequestDigest::from(&RequestDigest {
        system_hash: 0x1234_5678_9abc_def0,
        messages: 4,
        tools: 2,
        max_tokens: 1024,
        temperature: 0.2,
    })])
    .await;

    exercise(&[super::InferenceFailover::from(&FailoverRecord {
        stage: "plan".to_string(),
        iteration: 3,
        from_provider: "anthropic".to_string(),
        from_model: "claude-sonnet-4-5".to_string(),
        to_provider: "openai".to_string(),
        to_model: "gpt-5".to_string(),
        reason: "credits_exhausted".to_string(),
        kind: "insufficient_credits".to_string(),
        at: 201,
    })])
    .await;

    exercise(&[super::ModelRequest {
        record: run_archive::ModelInput {
            capture_status: run_archive::CaptureStatus::Retained,
            request: Some(serde_json::json!({ "model": "claude-sonnet-4-5" })),
            bytes: 43,
            source_context_digest: "0f0f0f0f0f0f0f0f".to_string(),
            parameters: std::collections::BTreeMap::new(),
            tool_catalog_version: "fedcba9876543210".to_string(),
            assembly_version: "1".to_string(),
        },
    }])
    .await;

    let RunRecord::InferenceAttempt(record) = attempt(
        1,
        "anthropic",
        "claude-sonnet-4-5",
        AttemptOutcome::Succeeded,
    ) else {
        unreachable!("attempt builds an attempt record")
    };
    let inference_attempt = super::InferenceAttempt {
        record,
        failover: None,
    };
    exercise(std::slice::from_ref(&inference_attempt)).await;
}
