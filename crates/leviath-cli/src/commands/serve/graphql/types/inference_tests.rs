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
        at: 200,
    })
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
                 total pageInfo { hasNextPage }
                 edges { node {
                   stage attempt provider model durationMs backoffMs at
                   outcome { kind failureKind transient capacity retry }
                   digest { systemHash messages tools maxTokens temperature }
                   failover {
                     stage iteration fromProvider fromModel toProvider toModel
                     reason failureKind at
                   }
                 } }
               } } }"#,
        )
        .await;
        let page = &json["run"]["inferences"];
        assert_eq!(page["total"], 2, "a move is not an attempt of its own");
        assert_eq!(page["pageInfo"]["hasNextPage"], false);

        let first = &page["edges"][0]["node"];
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
        let second = &page["edges"][1]["node"];
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
                "{ run { inferences(first: 10) { edges { node { \
                 outcome { kind failureKind transient capacity retry } \
                 failover { failureKind reason } } } } } }",
            )
            .await;
            let node = &json["run"]["inferences"]["edges"][0]["node"];
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
            "{ run { inferences(first: 2) { total pageInfo { hasNextPage endCursor } \
             edges { node { attempt outcome { retry } } } } } }",
        )
        .await;
        let page = &json["run"]["inferences"];
        assert_eq!(page["total"], 5);
        assert_eq!(page["pageInfo"]["hasNextPage"], true);
        assert_eq!(page["edges"][0]["node"]["attempt"], 1);
        assert_eq!(
            page["edges"][0]["node"]["outcome"]["retry"],
            "RENEWED_FILES"
        );
        let cursor = page["pageInfo"]["endCursor"].as_str().expect("a cursor");

        let json = data(&format!(
            r#"{{ run {{ inferences(first: 10, after: "{cursor}") {{
                 pageInfo {{ hasNextPage }} edges {{ node {{ attempt }} }}
               }} }} }}"#
        ))
        .await;
        let page = &json["run"]["inferences"];
        assert_eq!(page["pageInfo"]["hasNextPage"], false, "that was the rest");
        let numbers: Vec<i64> = page["edges"]
            .as_array()
            .expect("edges")
            .iter()
            .filter_map(|edge| edge["node"]["attempt"].as_i64())
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

/// A run that never called a provider has no attempts, and says so with an
/// empty page rather than an error.
#[tokio::test]
async fn a_run_that_never_called_a_provider_has_no_attempts() {
    crate::runstate::with_isolated_runs_dir_async("graphql-inferences-empty", |_dir| async move {
        create_run(&meta()).expect("run written");
        let json =
            data("{ run { inferences(first: 10) { total edges { node { provider } } } } }").await;
        assert_eq!(json["run"]["inferences"]["total"], 0);
        assert_eq!(
            json["run"]["inferences"]["edges"].as_array().map(Vec::len),
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
