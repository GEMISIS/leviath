//! Tests for why a run's regions changed: the contextChanges field and the
//! cause vocabulary behind it.
//!
//! These write a real journal and read it back through the schema, the same way
//! `interaction_tests.rs` does, because the whole surface is a reading of that
//! file.

use std::sync::Arc;

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};
use leviath_core::ContextCause;
use leviath_core::run_archive::{self, RunIdentity, RunRecord};

use super::super::run::Run;
use crate::commands::serve::testutil::state_with_agent_paths;
use crate::runstate::{RunMeta, create_run};

/// A run to hang a journal off.
fn meta() -> RunMeta {
    let mut meta = RunMeta::new(
        "moved-regions".to_string(),
        "coder".to_string(),
        "/agents/coder/agent.leviath".to_string(),
        "move a few regions".to_string(),
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

/// One region change record.
fn changed(
    region: &str,
    cause: ContextCause,
    added: usize,
    removed: usize,
    delta: i64,
    at: i64,
) -> RunRecord {
    RunRecord::ContextChange {
        region: region.to_string(),
        cause,
        entries_added: added,
        entries_removed: removed,
        token_delta: delta,
        at,
    }
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

/// Every field reads back typed, and a region that shrank carries a negative
/// token delta rather than an absolute one.
#[tokio::test]
async fn the_changes_read_back_typed_with_their_cause() {
    crate::runstate::with_isolated_runs_dir_async("graphql-context-changes", |_dir| async move {
        create_run(&meta()).expect("run written");
        write_journal(vec![
            changed("plan", ContextCause::Seed, 1, 0, 40, 20),
            changed("plan", ContextCause::Compaction, 0, 3, -120, 30),
        ]);

        let json = data(
            r#"{ run { contextChanges(first: 10) {
                 total pageInfo { hasNextPage }
                 edges { cursor node {
                   cause revisionBefore revisionAfter executionId journalPosition at
                   regions {
                     region digestBefore digestAfter tokensBefore tokensAfter
                     tokenDelta entriesBefore entriesAfter entriesAdded entriesRemoved
                   }
                 } }
               } } }"#,
        )
        .await;
        let page = &json["run"]["contextChanges"];
        assert_eq!(page["total"], 2);
        assert_eq!(page["pageInfo"]["hasNextPage"], false);

        let first = &page["edges"][0];
        assert_eq!(first["cursor"], "0");
        assert_eq!(first["node"]["cause"], "SEED");
        assert_eq!(first["node"]["at"], 20);
        assert_eq!(first["node"]["regions"][0]["region"], "plan");
        assert_eq!(first["node"]["regions"][0]["entriesAdded"], 1);
        assert_eq!(first["node"]["regions"][0]["entriesRemoved"], 0);
        assert_eq!(first["node"]["regions"][0]["tokenDelta"], 40);
        // A change recorded one region at a time named no window and digested
        // nothing, and says so rather than inventing either.
        assert!(first["node"]["revisionBefore"].is_null());
        assert!(first["node"]["revisionAfter"].is_null());
        assert!(first["node"]["executionId"].is_null());
        assert!(first["node"]["regions"][0]["digestBefore"].is_null());
        assert!(first["node"]["regions"][0]["tokensAfter"].is_null());
        assert!(first["node"]["regions"][0]["entriesBefore"].is_null());
        // The position of the record that carries it, which only climbs.
        let position = first["node"]["journalPosition"]
            .as_i64()
            .expect("a position");

        let second = &page["edges"][1]["node"];
        assert_eq!(second["cause"], "COMPACTION");
        assert_eq!(second["regions"][0]["entriesRemoved"], 3);
        assert_eq!(second["regions"][0]["tokenDelta"], -120);
        assert!(second["journalPosition"].as_i64().expect("a position") > position);
    })
    .await;
}

/// The snapshots and the reasons are two different fields of one journal: one
/// says what the window held, the other why it moved, and a change is not a
/// point in the history.
#[tokio::test]
async fn the_snapshots_and_the_reasons_are_separate_fields() {
    crate::runstate::with_isolated_runs_dir_async(
        "graphql-context-changes-vs-history",
        |_dir| async move {
            create_run(&meta()).expect("run written");
            write_journal(vec![
                RunRecord::ContextCheckpoint {
                    snapshot: leviath_core::run_meta::ContextSnapshot {
                        stage_name: "plan".to_string(),
                        total_tokens: 90,
                        max_tokens: 1_000,
                        regions: Vec::new(),
                    },
                    at: 25,
                },
                changed("conversation", ContextCause::ToolResult, 2, 0, 90, 25),
                changed("conversation", ContextCause::ModelReply, 1, 0, 30, 26),
            ]);

            let json = data(
                "{ run { contextChanges(first: 10) { total edges { node { cause } } } \
                 contextHistory(first: 10) { total } } }",
            )
            .await;
            assert_eq!(json["run"]["contextChanges"]["total"], 2);
            assert_eq!(
                json["run"]["contextChanges"]["edges"][0]["node"]["cause"],
                "TOOL_RESULT"
            );
            assert_eq!(
                json["run"]["contextChanges"]["edges"][1]["node"]["cause"],
                "MODEL_REPLY"
            );
            // The window was snapshotted once, so the history holds one point
            // however many reasons sit beside it.
            assert_eq!(json["run"]["contextHistory"]["total"], 1);
        },
    )
    .await;
}

/// The page carries on from its cursor, and the last page says it is the last.
#[tokio::test]
async fn the_changes_page_carries_on_from_its_cursor() {
    crate::runstate::with_isolated_runs_dir_async(
        "graphql-context-changes-paging",
        |_dir| async move {
            create_run(&meta()).expect("run written");
            write_journal(
                (0..5)
                    .map(|i| changed("plan", ContextCause::ContextTool, 1, 0, 10, 20 + i))
                    .collect(),
            );

            let json = data(
                "{ run { contextChanges(first: 2) { total pageInfo { hasNextPage endCursor } \
                 edges { node { at } } } } }",
            )
            .await;
            let page = &json["run"]["contextChanges"];
            assert_eq!(page["total"], 5);
            assert_eq!(page["pageInfo"]["hasNextPage"], true);
            assert_eq!(page["edges"][0]["node"]["at"], 20);
            let cursor = page["pageInfo"]["endCursor"].as_str().expect("a cursor");

            let json = data(&format!(
                r#"{{ run {{ contextChanges(first: 10, after: "{cursor}") {{
                     pageInfo {{ hasNextPage }} edges {{ node {{ at }} }}
                   }} }} }}"#
            ))
            .await;
            let page = &json["run"]["contextChanges"];
            assert_eq!(page["pageInfo"]["hasNextPage"], false, "that was the rest");
            let times: Vec<i64> = page["edges"]
                .as_array()
                .expect("edges")
                .iter()
                .filter_map(|edge| edge["node"]["at"].as_i64())
                .collect();
            assert_eq!(times, vec![22, 23, 24], "no change read twice");
        },
    )
    .await;
}

/// A page larger than the cap is refused, saying what the cap is.
#[tokio::test]
async fn a_page_over_the_cap_is_refused() {
    crate::runstate::with_isolated_runs_dir_async(
        "graphql-context-changes-cap",
        |_dir| async move {
            create_run(&meta()).expect("run written");
            let message = error("{ run { contextChanges(first: 5000) { total } } }").await;
            assert!(message.contains("at most 200"), "{message}");
            let message = error("{ run { contextChanges(first: 0) { total } } }").await;
            assert!(message.contains("at least 1"), "{message}");
        },
    )
    .await;
}

/// A cursor from another listing is refused rather than resumed somewhere else.
#[tokio::test]
async fn a_cursor_from_elsewhere_is_refused() {
    crate::runstate::with_isolated_runs_dir_async(
        "graphql-context-changes-cursor",
        |_dir| async move {
            create_run(&meta()).expect("run written");
            let message = error(
                r#"{ run { contextChanges(first: 2, after: "not-a-cursor-from-here") { total } } }"#,
            )
            .await;
            assert!(!message.is_empty(), "it says why");
        },
    )
    .await;
}

/// A run whose writes named no cause has no changes, and says so with an empty
/// page rather than an error.
#[tokio::test]
async fn a_run_with_no_recorded_causes_has_no_changes() {
    crate::runstate::with_isolated_runs_dir_async(
        "graphql-context-changes-empty",
        |_dir| async move {
            create_run(&meta()).expect("run written");
            let json =
                data("{ run { contextChanges(first: 10) { total edges { node { cause } } } } }")
                    .await;
            assert_eq!(json["run"]["contextChanges"]["total"], 0);
            assert_eq!(
                json["run"]["contextChanges"]["edges"]
                    .as_array()
                    .map(Vec::len),
                Some(0)
            );
        },
    )
    .await;
}

/// A journal that cannot be read is an error about the journal, not an empty
/// list that looks like a run whose regions never moved.
#[tokio::test]
async fn an_unreadable_journal_says_so() {
    crate::runstate::with_isolated_runs_dir_async(
        "graphql-context-changes-corrupt",
        |_dir| async move {
            create_run(&meta()).expect("run written");
            std::fs::write(
                crate::runstate::run_dir("moved-regions").join(leviath_core::files::ARCHIVE_FILE),
                b"not an archive",
            )
            .expect("a corrupt journal");
            let message = error("{ run { contextChanges(first: 10) { total } } }").await;
            assert!(message.contains("unreadable journal"), "{message}");
        },
    )
    .await;
}

/// Every cause maps to its own word, with no fallback swallowing one.
///
/// A handful are exercised end to end above; this pins the whole vocabulary,
/// which is the one thing a served enum can get silently wrong.
#[test]
fn every_cause_has_a_word() {
    use super::ContextCause as Served;
    let cases = [
        (ContextCause::Seed, Served::Seed),
        (ContextCause::Message, Served::Message),
        (ContextCause::ModelReply, Served::ModelReply),
        (ContextCause::ToolResult, Served::ToolResult),
        (ContextCause::ProducedPart, Served::ProducedPart),
        (ContextCause::Compaction, Served::Compaction),
        (ContextCause::Transform, Served::Transform),
        (ContextCause::ContextTool, Served::ContextTool),
        (ContextCause::Hook, Served::Hook),
        (ContextCause::FanOut, Served::FanOut),
        (ContextCause::Interaction, Served::Interaction),
        (ContextCause::Resume, Served::Resume),
        (ContextCause::Framework, Served::Framework),
    ];
    for (core, served) in cases {
        assert_eq!(Served::from(core), served, "{core:?}");
    }
}

/// A transaction reads back as one change naming every region it touched, and
/// carrying the window either side of it.
///
/// The join the whole debugger rests on: `revisionAfter` is what
/// `contextSnapshot` resolves to the content this change produced.
#[tokio::test]
async fn a_transaction_reads_back_with_the_windows_either_side() {
    crate::runstate::with_isolated_runs_dir_async(
        "graphql-context-transaction",
        |_dir| async move {
            create_run(&meta()).expect("run written");
            write_journal(vec![RunRecord::ContextTransaction {
                revision_before: "cw1-before".to_string(),
                revision_after: "cw1-after".to_string(),
                cause: ContextCause::Compaction,
                regions: vec![
                    run_archive::RegionCommit {
                        region: "plan".to_string(),
                        digest_before: "rg1-full".to_string(),
                        digest_after: "rg1-empty".to_string(),
                        tokens_before: 400,
                        tokens_after: 0,
                        entries_before: 4,
                        entries_after: 0,
                        entries_added: 0,
                    },
                    run_archive::RegionCommit {
                        region: "plan_history".to_string(),
                        digest_before: "rg1-empty".to_string(),
                        digest_after: "rg1-summary".to_string(),
                        tokens_before: 0,
                        tokens_after: 30,
                        entries_before: 0,
                        entries_after: 1,
                        entries_added: 1,
                    },
                ],
                execution_id: "x-one".to_string(),
                at: 90,
            }]);

            let json = data(
                r#"{ run { contextChanges(first: 10) {
                     total edges { node {
                       cause revisionBefore revisionAfter executionId
                       regions {
                         region digestBefore digestAfter tokensBefore tokensAfter
                         tokenDelta entriesBefore entriesAfter entriesAdded entriesRemoved
                       }
                     } }
                   } } }"#,
            )
            .await;
            let page = &json["run"]["contextChanges"];
            assert_eq!(page["total"], 1, "one transaction, one change");
            let node = &page["edges"][0]["node"];
            assert_eq!(node["cause"], "COMPACTION");
            assert_eq!(node["revisionBefore"], "cw1-before");
            assert_eq!(node["revisionAfter"], "cw1-after");
            assert_eq!(node["executionId"], "x-one");

            let emptied = &node["regions"][0];
            assert_eq!(emptied["region"], "plan");
            assert_eq!(emptied["digestBefore"], "rg1-full");
            assert_eq!(emptied["digestAfter"], "rg1-empty");
            assert_eq!(emptied["tokensBefore"], 400);
            assert_eq!(emptied["tokensAfter"], 0);
            assert_eq!(emptied["tokenDelta"], -400);
            assert_eq!(emptied["entriesBefore"], 4);
            assert_eq!(emptied["entriesAfter"], 0);
            assert_eq!(emptied["entriesRemoved"], 4);

            let summarised = &node["regions"][1];
            assert_eq!(summarised["region"], "plan_history");
            assert_eq!(summarised["tokenDelta"], 30);
            assert_eq!(summarised["entriesAdded"], 1);
            assert_eq!(summarised["entriesRemoved"], 0);
        },
    )
    .await;
}
