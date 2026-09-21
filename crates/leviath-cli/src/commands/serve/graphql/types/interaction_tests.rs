//! Tests for what a run asked: the interactions field and the settlement
//! typing behind it.
//!
//! These write a real journal and read it back through the schema, the same
//! way `execution_tests.rs` does, because the whole surface is a reading of
//! that file.

use std::sync::Arc;

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};
use leviath_core::interaction::{ApprovalScope, InteractionKind, Settlement};
use leviath_core::run_archive::{self, RunIdentity, RunRecord};

use super::super::run::Run;
use crate::commands::serve::testutil::state_with_agent_paths;
use crate::runstate::{RunMeta, create_run};

/// A run to hang a journal off.
fn meta() -> RunMeta {
    let mut meta = RunMeta::new(
        "asked-things".to_string(),
        "coder".to_string(),
        "/agents/coder/agent.leviath".to_string(),
        "ask a few things".to_string(),
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

/// One settled interaction record.
fn asked(
    request_id: &str,
    kind: InteractionKind,
    tool: Option<&str>,
    prompt: &str,
    settlement: Settlement,
) -> RunRecord {
    RunRecord::Interaction {
        request_id: request_id.to_string(),
        kind,
        tool: tool.map(str::to_string),
        prompt: prompt.to_string(),
        stage: "plan".to_string(),
        settlement,
        asked_at: 100,
        at: 105,
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

/// Every field reads back typed, and the settlement's detail fields are only
/// present where the ask was actually answered.
#[tokio::test]
async fn the_interactions_read_back_typed_with_their_settlement() {
    crate::runstate::with_isolated_runs_dir_async("graphql-interactions", |_dir| async move {
        create_run(&meta()).expect("run written");
        write_journal(vec![
            asked(
                "r1",
                InteractionKind::ToolApproval,
                Some("shell"),
                "Allow tool call: `shell`?",
                Settlement::Answered {
                    approved: Some(true),
                    scope: Some(ApprovalScope::Run),
                    choice: Some(2),
                    text: None,
                    feedback: None,
                },
            ),
            asked(
                "r2",
                InteractionKind::FreeText,
                None,
                "what next?",
                Settlement::TimedOut,
            ),
        ]);

        let json = data(
            r#"{ run { interactions(first: 10) {
                 total pageInfo { hasNextPage }
                 edges { node {
                   requestId kind tool prompt stage askedAt settledAt
                   settlement { outcome approved scope choice text feedback }
                 } }
               } } }"#,
        )
        .await;
        let page = &json["run"]["interactions"];
        assert_eq!(page["total"], 2);
        assert_eq!(page["pageInfo"]["hasNextPage"], false);

        let first = &page["edges"][0]["node"];
        assert_eq!(first["requestId"], "r1");
        assert_eq!(first["kind"], "TOOL_APPROVAL");
        assert_eq!(first["tool"], "shell");
        assert_eq!(first["prompt"], "Allow tool call: `shell`?");
        assert_eq!(first["stage"], "plan");
        assert_eq!(first["askedAt"], 100);
        assert_eq!(first["settledAt"], 105);
        assert_eq!(first["settlement"]["outcome"], "ANSWERED");
        assert_eq!(first["settlement"]["approved"], true);
        // The widest scope is spelled RUN here, not the REST wire's `session`.
        assert_eq!(first["settlement"]["scope"], "RUN");
        assert_eq!(first["settlement"]["choice"], 2);
        assert!(first["settlement"]["text"].is_null());
        assert!(first["settlement"]["feedback"].is_null());

        // Nobody answered the second ask, so every detail field is null: there
        // is nothing to read off a timeout beyond the fact that it happened.
        let second = &page["edges"][1]["node"];
        assert_eq!(second["kind"], "FREE_TEXT");
        assert!(second["tool"].is_null());
        assert_eq!(second["settlement"]["outcome"], "TIMED_OUT");
        assert!(second["settlement"]["approved"].is_null());
        assert!(second["settlement"]["scope"].is_null());
        assert!(second["settlement"]["choice"].is_null());
        assert!(second["settlement"]["text"].is_null());
        assert!(second["settlement"]["feedback"].is_null());
    })
    .await;
}

/// A cancelled ask settles with no detail either, the same as a timeout: the
/// request was withdrawn, not answered.
#[tokio::test]
async fn a_cancelled_ask_carries_no_answer_detail() {
    crate::runstate::with_isolated_runs_dir_async("graphql-interactions-cancel", |_dir| async move {
        create_run(&meta()).expect("run written");
        write_journal(vec![asked(
            "r1",
            InteractionKind::Confirm,
            None,
            "proceed?",
            Settlement::Cancelled,
        )]);

        let json =
            data("{ run { interactions(first: 10) { edges { node { settlement { outcome approved } } } } } }")
                .await;
        let node = &json["run"]["interactions"]["edges"][0]["node"];
        assert_eq!(node["settlement"]["outcome"], "CANCELLED");
        assert!(node["settlement"]["approved"].is_null());
    })
    .await;
}

/// A refused ask is its own outcome, not a denial: nobody decided anything, so
/// a reader who saw `ANSWERED` with `approved: false` would read a decision
/// into a fault in the server.
#[tokio::test]
async fn a_refused_ask_is_told_apart_from_a_denial() {
    crate::runstate::with_isolated_runs_dir_async(
        "graphql-interactions-refused",
        |_dir| async move {
            create_run(&meta()).expect("run written");
            write_journal(vec![asked(
                "r1",
                InteractionKind::ToolApproval,
                Some("bash"),
                "Allow tool call: `bash`?",
                Settlement::Refused,
            )]);

            let json = data(
                "{ run { interactions(first: 10) { edges { node { settlement { outcome approved \
             scope choice text feedback } } } } } }",
            )
            .await;
            let settlement = &json["run"]["interactions"]["edges"][0]["node"]["settlement"];
            assert_eq!(settlement["outcome"], "REFUSED");
            for field in ["approved", "scope", "choice", "text", "feedback"] {
                assert!(
                    settlement[field].is_null(),
                    "{field} is null on an ask nobody answered: {settlement}"
                );
            }
        },
    )
    .await;
}

/// A denial with feedback carries the redirect, and no approval.
#[tokio::test]
async fn a_denial_with_feedback_carries_it() {
    crate::runstate::with_isolated_runs_dir_async("graphql-interactions-deny", |_dir| async move {
        create_run(&meta()).expect("run written");
        write_journal(vec![asked(
            "r1",
            InteractionKind::ToolApproval,
            Some("bash"),
            "Allow tool call: `bash`?",
            Settlement::Answered {
                approved: Some(false),
                scope: Some(ApprovalScope::Once),
                choice: Some(4),
                text: None,
                feedback: Some("use git log instead".to_string()),
            },
        )]);

        let json = data(
            "{ run { interactions(first: 10) { edges { node { settlement { \
             approved scope feedback } } } } } }",
        )
        .await;
        let settlement = &json["run"]["interactions"]["edges"][0]["node"]["settlement"];
        assert_eq!(settlement["approved"], false);
        assert_eq!(settlement["scope"], "ONCE");
        assert_eq!(settlement["feedback"], "use git log instead");
    })
    .await;
}

/// The page carries on from its cursor, and the last page says it is the
/// last.
#[tokio::test]
async fn the_interactions_page_carries_on_from_its_cursor() {
    crate::runstate::with_isolated_runs_dir_async("graphql-interactions-paging", |_dir| async move {
        create_run(&meta()).expect("run written");
        write_journal(
            (0..5)
                .map(|i| {
                    asked(
                        &format!("r{i}"),
                        InteractionKind::Confirm,
                        None,
                        "ok?",
                        Settlement::TimedOut,
                    )
                })
                .collect(),
        );

        let json = data(
            "{ run { interactions(first: 2) { total pageInfo { hasNextPage endCursor } edges { node { requestId } } } } }",
        )
        .await;
        let page = &json["run"]["interactions"];
        assert_eq!(page["total"], 5);
        assert_eq!(page["pageInfo"]["hasNextPage"], true);
        assert_eq!(page["edges"][0]["node"]["requestId"], "r0");
        let cursor = page["pageInfo"]["endCursor"].as_str().expect("a cursor");

        let json = data(&format!(
            r#"{{ run {{ interactions(first: 10, after: "{cursor}") {{
                 pageInfo {{ hasNextPage }} edges {{ node {{ requestId }} }}
               }} }} }}"#
        ))
        .await;
        let page = &json["run"]["interactions"];
        assert_eq!(page["pageInfo"]["hasNextPage"], false, "that was the rest");
        let ids: Vec<&str> = page["edges"]
            .as_array()
            .expect("edges")
            .iter()
            .filter_map(|edge| edge["node"]["requestId"].as_str())
            .collect();
        assert_eq!(ids, vec!["r2", "r3", "r4"], "no interaction read twice");
    })
    .await;
}

/// A page larger than the cap is refused, saying what the cap is.
#[tokio::test]
async fn a_page_over_the_cap_is_refused() {
    crate::runstate::with_isolated_runs_dir_async("graphql-interactions-cap", |_dir| async move {
        create_run(&meta()).expect("run written");
        let message = error("{ run { interactions(first: 5000) { total } } }").await;
        assert!(message.contains("at most 200"), "{message}");
        let message = error("{ run { interactions(first: 0) { total } } }").await;
        assert!(message.contains("at least 1"), "{message}");
    })
    .await;
}

/// A cursor from another listing is refused rather than resumed somewhere
/// else.
#[tokio::test]
async fn a_cursor_from_elsewhere_is_refused() {
    crate::runstate::with_isolated_runs_dir_async(
        "graphql-interactions-cursor",
        |_dir| async move {
            create_run(&meta()).expect("run written");
            let message = error(
                r#"{ run { interactions(first: 2, after: "not-a-cursor-from-here") { total } } }"#,
            )
            .await;
            assert!(!message.is_empty(), "it says why");
        },
    )
    .await;
}

/// A run that never asked anybody anything has no interactions, and says so
/// with an empty page rather than an error.
#[tokio::test]
async fn a_run_that_never_asked_has_no_interactions() {
    crate::runstate::with_isolated_runs_dir_async(
        "graphql-interactions-empty",
        |_dir| async move {
            create_run(&meta()).expect("run written");
            let json =
                data("{ run { interactions(first: 10) { total edges { node { requestId } } } } }")
                    .await;
            assert_eq!(json["run"]["interactions"]["total"], 0);
            assert_eq!(
                json["run"]["interactions"]["edges"]
                    .as_array()
                    .map(Vec::len),
                Some(0)
            );
        },
    )
    .await;
}

/// A journal that cannot be read is an error about the journal, not an empty
/// history that looks like a run which never asked anything.
#[tokio::test]
async fn an_unreadable_journal_says_so() {
    crate::runstate::with_isolated_runs_dir_async(
        "graphql-interactions-corrupt",
        |_dir| async move {
            create_run(&meta()).expect("run written");
            std::fs::write(
                crate::runstate::run_dir("asked-things").join(leviath_core::files::ARCHIVE_FILE),
                b"not an archive",
            )
            .expect("a corrupt journal");
            let message = error("{ run { interactions(first: 10) { total } } }").await;
            assert!(message.contains("unreadable journal"), "{message}");
        },
    )
    .await;
}

/// Every approval scope maps to its own word, with no fallback swallowing
/// one.
///
/// `RUN` and `ONCE` are already exercised end to end above; `STAGE` has no
/// query of its own, since the mapping is the whole of what there is to get
/// wrong.
#[test]
fn every_approval_scope_has_a_word() {
    use super::SettledApprovalScope as Served;
    let cases = [
        (ApprovalScope::Once, Served::Once),
        (ApprovalScope::Stage, Served::Stage),
        (ApprovalScope::Run, Served::Run),
    ];
    for (core, served) in cases {
        assert_eq!(Served::from(core), served, "{core:?}");
    }
}
