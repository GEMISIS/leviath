//! Tests for what a run asked: the merged `interactions`/`openInteraction`
//! type, and the settlement typing behind it.
//!
//! These write a real journal and read it back through the schema, the same
//! way `execution_tests.rs` does, because the whole surface is a reading of
//! that file.

use std::sync::Arc;

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};
use leviath_core::interaction::{ApprovalScope, InteractionKind, Settlement};
use leviath_core::run_archive::{self, RunIdentity, RunRecord};

use super::super::super::connection::Connection;
use super::super::run::Run;
use super::{Interaction, InteractionFilter, InteractionOrder};
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
                 total cursor
                 results {
                   id kind toolName prompt stageName askedAt settledAt isRequired
                   settlement { outcome approved scope choice text feedback }
                 }
               } } }"#,
        )
        .await;
        let page = &json["run"]["interactions"];
        assert_eq!(page["total"], 2);
        assert!(page["cursor"].is_null(), "the only page");

        let first = &page["results"][0];
        assert_eq!(first["id"], "r1");
        assert_eq!(first["kind"], "TOOL_APPROVAL");
        assert_eq!(first["toolName"], "shell");
        assert_eq!(first["prompt"], "Allow tool call: `shell`?");
        assert_eq!(first["stageName"], "plan");
        assert_eq!(first["askedAt"], 100);
        assert_eq!(first["settledAt"], 105);
        // The journal keeps no record of whether a settled ask was required.
        assert_eq!(first["isRequired"], false);
        assert_eq!(first["settlement"]["outcome"], "ANSWERED");
        assert_eq!(first["settlement"]["approved"], true);
        // The widest scope is spelled RUN here, not the REST wire's `session`.
        assert_eq!(first["settlement"]["scope"], "RUN");
        assert_eq!(first["settlement"]["choice"], 2);
        assert!(first["settlement"]["text"].is_null());
        assert!(first["settlement"]["feedback"].is_null());

        // Nobody answered the second ask, so every detail field is null: there
        // is nothing to read off a timeout beyond the fact that it happened.
        let second = &page["results"][1];
        assert_eq!(second["kind"], "FREE_TEXT");
        assert!(second["toolName"].is_null());
        assert_eq!(second["settlement"]["outcome"], "TIMED_OUT");
        assert!(second["settlement"]["approved"].is_null());
        assert!(second["settlement"]["scope"].is_null());
        assert!(second["settlement"]["choice"].is_null());
        assert!(second["settlement"]["text"].is_null());
        assert!(second["settlement"]["feedback"].is_null());
    })
    .await;
}

/// A settled ask carries no body, no options and no typed tool call: the
/// journal never kept them, so they read as absent rather than as a stale
/// echo of the open ask.
#[tokio::test]
async fn a_settled_ask_carries_none_of_the_open_only_fields() {
    crate::runstate::with_isolated_runs_dir_async("graphql-interactions-settled-shape", |_dir| async move {
        create_run(&meta()).expect("run written");
        write_journal(vec![asked(
            "r1",
            InteractionKind::MultipleChoice,
            None,
            "which one?",
            Settlement::TimedOut,
        )]);

        let json = data(
            "{ run { interactions(first: 10) { results { body options toolCall { toolName } } } } }",
        )
        .await;
        let node = &json["run"]["interactions"]["results"][0];
        assert!(node["body"].is_null());
        assert_eq!(node["options"].as_array().map(Vec::len), Some(0));
        assert!(node["toolCall"].is_null());
    })
    .await;
}

/// A settled interaction's `run` field answers with the run it belongs to.
#[tokio::test]
async fn a_settled_interaction_names_its_own_run() {
    crate::runstate::with_isolated_runs_dir_async("graphql-interactions-run", |_dir| async move {
        create_run(&meta()).expect("run written");
        write_journal(vec![asked(
            "r1",
            InteractionKind::Confirm,
            None,
            "proceed?",
            Settlement::TimedOut,
        )]);

        let json = data("{ run { interactions(first: 10) { results { run { id } } } } }").await;
        assert_eq!(
            json["run"]["interactions"]["results"][0]["run"]["id"],
            "asked-things"
        );
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
            data("{ run { interactions(first: 10) { results { settlement { outcome approved } } } } }")
                .await;
        let node = &json["run"]["interactions"]["results"][0];
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
                "{ run { interactions(first: 10) { results { settlement { outcome approved \
             scope choice text feedback } } } } }",
            )
            .await;
            let settlement = &json["run"]["interactions"]["results"][0]["settlement"];
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
            "{ run { interactions(first: 10) { results { settlement { \
             approved scope feedback } } } } }",
        )
        .await;
        let settlement = &json["run"]["interactions"]["results"][0]["settlement"];
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
    crate::runstate::with_isolated_runs_dir_async(
        "graphql-interactions-paging",
        |_dir| async move {
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

            let json =
                data("{ run { interactions(first: 2) { total cursor results { id } } } }").await;
            let page = &json["run"]["interactions"];
            assert_eq!(page["total"], 5);
            assert!(page["cursor"].as_str().is_some(), "more to come");
            assert_eq!(page["results"][0]["id"], "r0");
            let cursor = page["cursor"].as_str().expect("a cursor");

            let json = data(&format!(
                r#"{{ run {{ interactions(first: 10, after: "{cursor}") {{
                 cursor results {{ id }}
               }} }} }}"#
            ))
            .await;
            let page = &json["run"]["interactions"];
            assert!(page["cursor"].is_null(), "that was the rest");
            let ids: Vec<&str> = page["results"]
                .as_array()
                .expect("results")
                .iter()
                .filter_map(|node| node["id"].as_str())
                .collect();
            assert_eq!(ids, vec!["r2", "r3", "r4"], "no interaction read twice");
        },
    )
    .await;
}

/// Descending order walks the same five asks newest first.
#[tokio::test]
async fn descending_order_walks_newest_first() {
    crate::runstate::with_isolated_runs_dir_async("graphql-interactions-desc", |_dir| async move {
        create_run(&meta()).expect("run written");
        write_journal(
            (0..3)
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
            "{ run { interactions(orderBy: [{ field: SEQUENCE, direction: DESC }]) { \
             results { id } } } }",
        )
        .await;
        let ids: Vec<&str> = json["run"]["interactions"]["results"]
            .as_array()
            .expect("results")
            .iter()
            .filter_map(|node| node["id"].as_str())
            .collect();
        assert_eq!(ids, vec!["r2", "r1", "r0"]);
    })
    .await;
}

/// A filter narrows which asks come back.
#[tokio::test]
async fn a_filter_narrows_the_listing() {
    crate::runstate::with_isolated_runs_dir_async(
        "graphql-interactions-filter",
        |_dir| async move {
            create_run(&meta()).expect("run written");
            write_journal(vec![
                asked(
                    "r1",
                    InteractionKind::Confirm,
                    None,
                    "ok?",
                    Settlement::TimedOut,
                ),
                asked(
                    "r2",
                    InteractionKind::ToolApproval,
                    Some("shell"),
                    "run it?",
                    Settlement::TimedOut,
                ),
            ]);

            let json = data(
                r#"{ run { interactions(filter: { kind: { eq: TOOL_APPROVAL } }) {
                 total results { id }
               } } }"#,
            )
            .await;
            let page = &json["run"]["interactions"];
            assert_eq!(page["total"], 1);
            assert_eq!(page["results"][0]["id"], "r2");
        },
    )
    .await;
}

/// A root that hands out a fixed set of open asks, the way the daemon's own
/// memory would.
struct OpenProbe {
    open: Vec<(String, leviath_core::interaction::InteractionRequest)>,
}

#[async_graphql::Object]
impl OpenProbe {
    /// The approval inbox, over the fixed set this probe was given.
    async fn open_interactions(
        &self,
        filter: Option<InteractionFilter>,
        order_by: Option<Vec<InteractionOrder>>,
        #[graphql(default = 50)] first: i32,
        after: Option<crate::commands::serve::graphql::scalars::Cursor>,
    ) -> async_graphql::Result<Connection<Interaction>> {
        super::open(self.open.clone(), filter, order_by, first, after).await
    }
}

/// One open ask, for the approval-inbox tests.
fn open_ask(
    run: &str,
    id: &str,
    kind: InteractionKind,
) -> (String, leviath_core::interaction::InteractionRequest) {
    (
        run.to_string(),
        leviath_core::interaction::InteractionRequest {
            id: id.to_string(),
            kind,
            prompt: format!("what next for {run}?"),
            options: Vec::new(),
            tool_name: None,
            tool_arguments: None,
            required: true,
            stage_name: "plan".to_string(),
            body: None,
            body_format: Default::default(),
        },
    )
}

/// The approval inbox walks the daemon's own open asks the same way a run's
/// settled ones are walked: filtered, ordered, paged and cursor-bound.
#[tokio::test]
async fn the_open_listing_walks_filters_orders_and_pages() {
    let open = vec![
        open_ask("run-a", "ask-1", InteractionKind::FreeText),
        open_ask("run-b", "ask-2", InteractionKind::ToolApproval),
        open_ask("run-c", "ask-3", InteractionKind::FreeText),
    ];
    let schema = Schema::build(OpenProbe { open }, EmptyMutation, EmptySubscription).finish();
    let ask = |query: String| {
        let schema = schema.clone();
        async move {
            let answer = schema.execute(Request::new(query)).await;
            assert!(answer.errors.is_empty(), "{:?}", answer.errors);
            serde_json::to_value(&answer.data).expect("data serializes")
        }
    };

    // The plain walk, in the order the daemon handed them over, with a total.
    let json =
        ask("{ openInteractions(first: 2) { total cursor results { id } } }".to_string()).await;
    let page = &json["openInteractions"];
    assert_eq!(page["total"], 3);
    let ids: Vec<&str> = page["results"]
        .as_array()
        .expect("results")
        .iter()
        .filter_map(|node| node["id"].as_str())
        .collect();
    assert_eq!(ids, vec!["ask-1", "ask-2"]);
    let cursor = page["cursor"].as_str().expect("more to come").to_string();

    // The cursor resumes the rest.
    let json = ask(format!(
        r#"{{ openInteractions(first: 10, after: "{cursor}") {{ cursor results {{ id }} }} }}"#
    ))
    .await;
    let page = &json["openInteractions"];
    assert!(page["cursor"].is_null(), "that was the rest");
    let ids: Vec<&str> = page["results"]
        .as_array()
        .expect("results")
        .iter()
        .filter_map(|node| node["id"].as_str())
        .collect();
    assert_eq!(ids, vec!["ask-3"], "no ask read twice");

    // Reversed order is the same three the other way round.
    let json = ask(
        "{ openInteractions(orderBy: [{ field: SEQUENCE, direction: DESC }]) { results { id } } }"
            .to_string(),
    )
    .await;
    let ids: Vec<&str> = json["openInteractions"]["results"]
        .as_array()
        .expect("results")
        .iter()
        .filter_map(|node| node["id"].as_str())
        .collect();
    assert_eq!(ids, vec!["ask-3", "ask-2", "ask-1"]);

    // A filter narrows the inbox the same way it narrows a run's settled asks.
    let json = ask(
        "{ openInteractions(filter: { kind: { eq: TOOL_APPROVAL } }) { total results { id } } }"
            .to_string(),
    )
    .await;
    let page = &json["openInteractions"];
    assert_eq!(page["total"], 1);
    assert_eq!(page["results"][0]["id"], "ask-2");
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

/// A cursor minted for one run's interactions is refused on another run's
/// listing, even though both use the same digest namespace.
#[tokio::test]
async fn a_cursor_from_another_run_is_refused() {
    crate::runstate::with_isolated_runs_dir_async(
        "graphql-interactions-foreign-run",
        |_dir| async move {
            create_run(&meta()).expect("run written");
            write_journal(
                (0..3)
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
            let mut other = meta();
            other.run_id = "another-run".to_string();
            create_run(&other).expect("a second run written");

            let json = data("{ run { interactions(first: 1) { cursor } } }").await;
            let cursor = json["run"]["interactions"]["cursor"]
                .as_str()
                .expect("a cursor")
                .to_string();

            // Read the cursor back against a schema whose run id differs, which
            // changes the digest the cursor is bound to.
            let run = Run {
                meta: Arc::new(other),
                now: 1_788_925_000,
            };
            let schema = Schema::build(Probe { run }, EmptyMutation, EmptySubscription)
                .data(state_with_agent_paths(Vec::new()))
                .finish();
            let answer = schema
                .execute(Request::new(format!(
                    r#"{{ run {{ interactions(first: 10, after: "{cursor}") {{ total }} }} }}"#
                )))
                .await;
            assert!(
                !answer.errors.is_empty(),
                "a cursor from another run's listing"
            );
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
            let json = data("{ run { interactions(first: 10) { total results { id } } } }").await;
            assert_eq!(json["run"]["interactions"]["total"], 0);
            assert_eq!(
                json["run"]["interactions"]["results"]
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
    use super::ApprovalScope as Served;
    let cases = [
        (ApprovalScope::Once, Served::Once),
        (ApprovalScope::Stage, Served::Stage),
        (ApprovalScope::Run, Served::Run),
    ];
    for (core, served) in cases {
        assert_eq!(Served::from(core), served, "{core:?}");
        // And back: the write side and the read side share one enum now.
        assert_eq!(ApprovalScope::from(served), core, "{served:?}");
    }
}

/// `openInteraction`, on a run parked on a person, answers with the pending
/// ask; `interactions` does not carry it yet, because a question is written
/// down only once it settles.
#[tokio::test]
async fn open_and_settled_asks_read_from_different_places() {
    crate::runstate::with_isolated_runs_dir_async("graphql-interactions-open", |_dir| async move {
        create_run(&meta()).expect("run written");
        // A run that never asked anybody anything has no *settled* asks,
        // read straight from its own journal: no daemon involved.
        let json = data("{ run { interactions(first: 10) { total } } }").await;
        assert_eq!(json["run"]["interactions"]["total"], 0);

        // `openInteraction` is the daemon's own memory, not the journal, so
        // a schema built with no control connection cannot answer it: this
        // is what tells the two fields apart rather than one silently
        // reading the other's absence.
        let message = error("{ run { openInteraction { id } } }").await;
        assert!(message.contains("Daemon not reachable"), "{message}");
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
        super::InteractionKind::FreeText,
        super::InteractionKind::ToolApproval,
    ])
    .await;
    exercise_enum(&[super::ApprovalScope::Once, super::ApprovalScope::Run]).await;
    exercise_enum(&[
        super::SettlementOutcome::Answered,
        super::SettlementOutcome::Refused,
    ])
    .await;

    exercise(&[
        super::Settlement::from(&Settlement::Answered {
            approved: Some(true),
            scope: Some(ApprovalScope::Run),
            choice: Some(2),
            text: None,
            feedback: None,
        }),
        super::Settlement::from(&Settlement::TimedOut),
    ])
    .await;

    let request = leviath_core::interaction::InteractionRequest {
        id: "ask-1".to_string(),
        kind: InteractionKind::FreeText,
        prompt: "what next?".to_string(),
        options: Vec::new(),
        tool_name: None,
        tool_arguments: None,
        required: true,
        stage_name: "plan".to_string(),
        body: None,
        body_format: Default::default(),
    };
    let open = super::Interaction::open("asked-things".to_string(), request);
    assert_eq!(open.id, "ask-1");
    assert_eq!(open.kind, super::InteractionKind::FreeText);
    assert_eq!(open.stage_name, "plan");
    assert!(open.is_required);
    exercise(std::slice::from_ref(&open)).await;

    let settled = super::Interaction::settled(
        "asked-things".to_string(),
        run_archive::InteractionRecord {
            request_id: "r1".to_string(),
            kind: InteractionKind::FreeText,
            tool: None,
            prompt: "what next?".to_string(),
            stage: "plan".to_string(),
            settlement: Settlement::TimedOut,
            asked_at: 100,
            at: 105,
        },
    );
    exercise(std::slice::from_ref(&settled)).await;
}

/// An ask that is still open and names a tool carries the call itself, typed,
/// as well as the tool's name.
///
/// A tool approval is the one open ask a console has to render the call for: a
/// person approving `shell` is approving a command line, and the name alone
/// does not say which.
#[test]
fn an_open_tool_approval_carries_the_call_it_is_about() {
    use leviath_core::interaction::{BodyFormat, InteractionRequest};

    let asked = Interaction::open(
        "run-a".to_string(),
        InteractionRequest {
            id: "req-1".to_string(),
            kind: InteractionKind::ToolApproval,
            prompt: "run this?".to_string(),
            options: Vec::new(),
            tool_name: Some("shell".to_string()),
            tool_arguments: Some(serde_json::json!({ "command": "ls -la" })),
            required: true,
            stage_name: "work".to_string(),
            body: None,
            body_format: BodyFormat::default(),
        },
    );
    assert_eq!(asked.tool_name.as_deref(), Some("shell"));
    assert!(
        asked.tool_call.is_some(),
        "the call the approval is about, typed"
    );
    assert!(
        asked.settlement.is_none() && asked.settled_at.is_none(),
        "it is still open"
    );

    // A request that names a tool and no arguments is a call with none, which
    // is what an empty object says.
    let bare = Interaction::open(
        "run-a".to_string(),
        InteractionRequest {
            id: "req-2".to_string(),
            kind: InteractionKind::ToolApproval,
            prompt: "run this?".to_string(),
            options: Vec::new(),
            tool_name: Some("shell".to_string()),
            tool_arguments: None,
            required: false,
            stage_name: "work".to_string(),
            body: None,
            body_format: BodyFormat::default(),
        },
    );
    assert!(bare.tool_call.is_some());
    assert!(!bare.is_required);
}
