//! Tests for what a run did: the executions field and the results behind it.
//!
//! These write a real journal and read it back through the schema, because the
//! whole surface is a reading of that file: the pairing of a dispatch with its
//! completion, the position a result is fetched by, and the states an attempt can
//! be left in.

use std::sync::Arc;

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};
use leviath_core::execution::ToolOutcome;
use leviath_core::run_archive::{self, RunIdentity, RunRecord, ToolCallRecord};

use super::run::Run;
use crate::commands::serve::testutil::state_with_agent_paths;
use crate::runstate::{RunMeta, create_run};

/// A run to hang a journal off.
fn meta() -> RunMeta {
    let mut meta = RunMeta::new(
        "did-things".to_string(),
        "coder".to_string(),
        "/agents/coder/agent.leviath".to_string(),
        "do two things".to_string(),
        None,
        "/tmp".to_string(),
        2,
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

/// One dispatched call.
fn call(id: &str, execution: &str, tool: &str, arguments: &str) -> ToolCallRecord {
    ToolCallRecord {
        execution_id: execution.to_string(),
        id: id.to_string(),
        name: tool.to_string(),
        arguments: arguments.to_string(),
        result: None,
        thought_signature: None,
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

/// Every attempt comes back with its call typed, its outcome and its result.
#[tokio::test]
async fn the_executions_read_back_typed_with_their_results() {
    crate::runstate::with_isolated_runs_dir_async("graphql-executions", |_dir| async move {
        create_run(&meta()).expect("run written");
        write_journal(vec![
            RunRecord::ToolBatch {
                calls: vec![
                    call("c1", "x1", "read_file", r#"{"path":"notes.md"}"#),
                    call("c2", "x2", "shell", r#"{"command":"ls -la"}"#),
                ],
                at: 100,
                stage_index: 1,
                iteration: 3,
                response: "reading then listing".to_string(),
            },
            RunRecord::ToolCallDone {
                iteration: 3,
                call_id: "c1".to_string(),
                execution_id: "x1".to_string(),
                result: "the notes".into(),
                outcome: Some(ToolOutcome::Succeeded),
                at: 101,
            },
        ]);

        let json = data(
            r#"{ run { executions(first: 10) {
                 total pageInfo { hasNextPage }
                 edges { node {
                   id callId outcome stageIndex iteration dispatchedAt endedAt journalPosition
                   call {
                     __typename toolName rawArguments
                     ... on ReadFileCall { args { path } }
                     ... on ShellCall { args { command } }
                   }
                   result { text bytes truncated parts }
                 } }
               } } }"#,
        )
        .await;
        let page = &json["run"]["executions"];
        assert_eq!(page["total"], 2);
        assert_eq!(page["pageInfo"]["hasNextPage"], false);

        // Dispatch order, so the read comes first even though the shell call has
        // no ending recorded.
        let first = &page["edges"][0]["node"];
        assert_eq!(first["id"], "x1");
        assert_eq!(first["callId"], "c1");
        assert_eq!(first["outcome"], "SUCCEEDED");
        assert_eq!(first["stageIndex"], 1);
        assert_eq!(first["iteration"], 3);
        assert_eq!(first["dispatchedAt"], 100);
        assert_eq!(first["endedAt"], 101);
        assert_eq!(first["call"]["__typename"], "ReadFileCall");
        assert_eq!(first["call"]["args"]["path"], "notes.md");
        // The raw arguments sit beside the typed reading of them, always.
        assert_eq!(first["call"]["rawArguments"]["path"], "notes.md");
        assert_eq!(first["result"]["text"], "the notes");
        assert_eq!(first["result"]["bytes"], 9);
        assert_eq!(first["result"]["truncated"], false);
        assert_eq!(first["result"]["parts"].as_array().map(Vec::len), Some(0));
        // Both calls of one batch name the same position, which is the record
        // that dispatched them.
        assert_eq!(
            first["journalPosition"], page["edges"][1]["node"]["journalPosition"],
            "one batch, one dispatch record"
        );

        // The second call never finished, and every field about its ending says
        // so rather than guessing.
        let second = &page["edges"][1]["node"];
        assert_eq!(second["call"]["__typename"], "ShellCall");
        assert_eq!(second["call"]["args"]["command"], "ls -la");
        assert!(second["endedAt"].is_null());
        assert!(second["outcome"].is_null());
        assert!(second["result"].is_null(), "nothing was recorded to read");
    })
    .await;
}

/// An attempt a resume gave up on reads as indeterminate, with the stand-in the
/// run carried on with.
///
/// This is the pairing the whole outcome field exists for: without it this
/// execution is indistinguishable from the one above that is still running.
#[tokio::test]
async fn an_abandoned_attempt_reads_as_indeterminate() {
    crate::runstate::with_isolated_runs_dir_async("graphql-indeterminate", |_dir| async move {
        create_run(&meta()).expect("run written");
        write_journal(vec![
            RunRecord::ToolBatch {
                calls: vec![call("c1", "x1", "shell", r#"{"command":"make"}"#)],
                at: 100,
                stage_index: 0,
                iteration: 1,
                response: String::new(),
            },
            RunRecord::ToolCallDone {
                iteration: 1,
                call_id: "c1".to_string(),
                execution_id: "x1".to_string(),
                result: "[error] interrupted: the daemon restarted".into(),
                outcome: Some(ToolOutcome::Indeterminate),
                at: 140,
            },
        ]);

        let json = data(
            "{ run { executions(first: 10) { edges { node { outcome endedAt result { text } } } } } }",
        )
        .await;
        let node = &json["run"]["executions"]["edges"][0]["node"];
        assert_eq!(node["outcome"], "INDETERMINATE");
        assert_eq!(node["endedAt"], 140);
        assert!(
            node["result"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("interrupted")),
            "the stand-in the run went on with: {}",
            node["result"]["text"]
        );
    })
    .await;
}

/// A call the dispatcher resolved before it reached the lane carries its result
/// in the batch record, and reads back from there.
#[tokio::test]
async fn an_inline_result_reads_from_its_batch_record() {
    crate::runstate::with_isolated_runs_dir_async("graphql-inline", |_dir| async move {
        create_run(&meta()).expect("run written");
        let mut refused = call("c1", "x1", "shell", r#"{"command":"curl evil"}"#);
        refused.result = Some("[blocked] taint gate refused it".into());
        write_journal(vec![RunRecord::ToolBatch {
            calls: vec![refused],
            at: 100,
            stage_index: 0,
            iteration: 1,
            response: String::new(),
        }]);

        let json = data(
            "{ run { executions(first: 10) { edges { node { endedAt result { text } } } } } }",
        )
        .await;
        let node = &json["run"]["executions"]["edges"][0]["node"];
        assert_eq!(node["endedAt"], 100, "it ended when it was dispatched");
        assert_eq!(node["result"]["text"], "[blocked] taint gate refused it");
    })
    .await;
}

/// The page carries on from its cursor, and the last page says it is the last.
#[tokio::test]
async fn the_executions_page_carries_on_from_its_cursor() {
    crate::runstate::with_isolated_runs_dir_async("graphql-exec-paging", |_dir| async move {
        create_run(&meta()).expect("run written");
        write_journal(vec![RunRecord::ToolBatch {
            calls: (0..5)
                .map(|i| {
                    call(
                        &format!("c{i}"),
                        &format!("x{i}"),
                        "read_file",
                        r#"{"path":"a.txt"}"#,
                    )
                })
                .collect(),
            at: 100,
            stage_index: 0,
            iteration: 1,
            response: String::new(),
        }]);

        let json = data(
            "{ run { executions(first: 2) { total pageInfo { hasNextPage endCursor } edges { node { callId } } } } }",
        )
        .await;
        let page = &json["run"]["executions"];
        assert_eq!(page["total"], 5);
        assert_eq!(page["pageInfo"]["hasNextPage"], true);
        assert_eq!(page["edges"][0]["node"]["callId"], "c0");
        let cursor = page["pageInfo"]["endCursor"].as_str().expect("a cursor");

        let json = data(&format!(
            r#"{{ run {{ executions(first: 10, after: "{cursor}") {{
                 pageInfo {{ hasNextPage }} edges {{ node {{ callId }} }}
               }} }} }}"#
        ))
        .await;
        let page = &json["run"]["executions"];
        assert_eq!(page["pageInfo"]["hasNextPage"], false, "that was the rest");
        let ids: Vec<&str> = page["edges"]
            .as_array()
            .expect("edges")
            .iter()
            .filter_map(|edge| edge["node"]["callId"].as_str())
            .collect();
        assert_eq!(ids, vec!["c2", "c3", "c4"], "no call read twice");
    })
    .await;
}

/// A page larger than the cap is refused, saying what the cap is.
#[tokio::test]
async fn a_page_over_the_cap_is_refused() {
    crate::runstate::with_isolated_runs_dir_async("graphql-exec-cap", |_dir| async move {
        create_run(&meta()).expect("run written");
        let message = error("{ run { executions(first: 5000) { total } } }").await;
        assert!(message.contains("at most 200"), "{message}");
        let message = error("{ run { executions(first: 0) { total } } }").await;
        assert!(message.contains("at least 1"), "{message}");
    })
    .await;
}

/// A cursor from another listing is refused rather than resumed somewhere else.
#[tokio::test]
async fn a_cursor_from_elsewhere_is_refused() {
    crate::runstate::with_isolated_runs_dir_async("graphql-exec-cursor", |_dir| async move {
        create_run(&meta()).expect("run written");
        let message =
            error(r#"{ run { executions(first: 2, after: "not-a-cursor-from-here") { total } } }"#)
                .await;
        assert!(!message.is_empty(), "it says why");
    })
    .await;
}

/// A run that never dispatched a tool did nothing, and says so with an empty
/// page rather than an error.
///
/// A run with no journal at all reads the same way: there is a difference between
/// "this run is unknown", which the run field answers, and "this run did
/// nothing", which is this.
#[tokio::test]
async fn a_run_that_did_nothing_has_no_executions() {
    crate::runstate::with_isolated_runs_dir_async("graphql-exec-empty", |_dir| async move {
        create_run(&meta()).expect("run written");
        let json =
            data("{ run { executions(first: 10) { total edges { node { callId } } } } }").await;
        assert_eq!(json["run"]["executions"]["total"], 0);
        assert_eq!(
            json["run"]["executions"]["edges"].as_array().map(Vec::len),
            Some(0)
        );
    })
    .await;
}

/// A result larger than the cap comes back as its head, saying so and saying how
/// big the whole thing is.
#[tokio::test]
async fn a_large_result_comes_back_as_its_head() {
    crate::runstate::with_isolated_runs_dir_async("graphql-exec-large", |_dir| async move {
        create_run(&meta()).expect("run written");
        let whole = "x".repeat(crate::commands::serve::core::executions::RESULT_MAX_BYTES + 500);
        write_journal(vec![
            RunRecord::ToolBatch {
                calls: vec![call("c1", "x1", "read_file", r#"{"path":"big.txt"}"#)],
                at: 100,
                stage_index: 0,
                iteration: 1,
                response: String::new(),
            },
            RunRecord::ToolCallDone {
                iteration: 1,
                call_id: "c1".to_string(),
                execution_id: "x1".to_string(),
                result: whole.clone().into(),
                outcome: Some(ToolOutcome::Succeeded),
                at: 101,
            },
        ]);

        let json =
            data("{ run { executions(first: 1) { edges { node { result { text bytes truncated } } } } } }")
                .await;
        let result = &json["run"]["executions"]["edges"][0]["node"]["result"];
        assert_eq!(result["truncated"], true);
        assert_eq!(result["bytes"], whole.len());
        assert_eq!(
            result["text"].as_str().map(str::len),
            Some(crate::commands::serve::core::executions::RESULT_MAX_BYTES),
            "the head, up to the cap"
        );
    })
    .await;
}

/// A journal that is not readable is an error about the journal, not an empty
/// history that looks like a run which did nothing.
#[tokio::test]
async fn an_unreadable_journal_says_so() {
    crate::runstate::with_isolated_runs_dir_async("graphql-exec-corrupt", |_dir| async move {
        create_run(&meta()).expect("run written");
        std::fs::write(
            crate::runstate::run_dir("did-things").join(leviath_core::files::ARCHIVE_FILE),
            b"not an archive",
        )
        .expect("a corrupt journal");
        let message = error("{ run { executions(first: 10) { total } } }").await;
        assert!(message.contains("unreadable journal"), "{message}");
    })
    .await;
}

/// Every outcome the journal can hold has a word on the wire.
///
/// Five states, mapped one to one. A state this build could not name would have
/// to come through as null, which already means three other things.
#[test]
fn every_outcome_has_a_word() {
    use super::execution::ToolOutcome as Served;
    let cases = [
        (ToolOutcome::Succeeded, Served::Succeeded),
        (ToolOutcome::Failed, Served::Failed),
        (ToolOutcome::Blocked, Served::Blocked),
        (ToolOutcome::Denied, Served::Denied),
        (ToolOutcome::Indeterminate, Served::Indeterminate),
    ];
    for (recorded, served) in cases {
        assert_eq!(Served::from(recorded), served, "{recorded:?}");
    }
}

/// A result that carried stored parts names them.
///
/// The bytes are not repeated here: they are already in the run's parts, and a
/// history that inlined every attached file would be the one thing this surface
/// is built to avoid.
#[tokio::test]
async fn a_result_with_stored_parts_names_them() {
    crate::runstate::with_isolated_runs_dir_async("graphql-exec-parts", |_dir| async move {
        create_run(&meta()).expect("run written");
        let part = leviath_core::mime::Part::stored(leviath_core::mime::BlobRef {
            sha256: "abc123".to_string(),
            mime_type: leviath_core::mime::MimeType::parse("image/png").expect("a type"),
            size: 12,
            width: None,
            height: None,
            duration_ms: None,
            tokens: 1,
            stand_in: "[image/png, 12 B] diagram.png".to_string(),
        })
        .named("diagram.png");
        let carried = leviath_core::region::EntryContent::from_parts(vec![
            leviath_core::mime::Part::text("here it is"),
            part,
        ]);
        write_journal(vec![
            RunRecord::ToolBatch {
                calls: vec![call(
                    "c1",
                    "x1",
                    "context_attach",
                    r#"{"region":"n","path":"d.png"}"#,
                )],
                at: 100,
                stage_index: 0,
                iteration: 1,
                response: String::new(),
            },
            RunRecord::ToolCallDone {
                iteration: 1,
                call_id: "c1".to_string(),
                execution_id: "x1".to_string(),
                result: carried,
                outcome: Some(ToolOutcome::Succeeded),
                at: 101,
            },
        ]);

        let json =
            data("{ run { executions(first: 1) { edges { node { result { text parts } } } } } }")
                .await;
        let result = &json["run"]["executions"]["edges"][0]["node"]["result"];
        assert_eq!(
            result["parts"].as_array().and_then(|p| p.first()),
            Some(&serde_json::json!("diagram.png"))
        );
        assert!(
            result["text"]
                .as_str()
                .is_some_and(|t| t.contains("here it is")),
            "the text renders beside the part: {}",
            result["text"]
        );
    })
    .await;
}

/// Reading one result from a journal that broke since the page was read fails,
/// rather than reading as an attempt that produced nothing.
#[tokio::test]
async fn a_result_read_from_a_broken_journal_fails() {
    crate::runstate::with_isolated_runs_dir_async(
        "graphql-exec-result-broken",
        |_dir| async move {
            create_run(&meta()).expect("run written");
            std::fs::write(
                crate::runstate::run_dir("did-things").join(leviath_core::files::ARCHIVE_FILE),
                b"not an archive",
            )
            .expect("a corrupt journal");

            // The execution is built here rather than listed, because listing is what
            // would fail first: this is the race where the file breaks in between.
            let execution = super::execution::ToolExecution {
                run_id: "did-things".to_string(),
                record: leviath_core::run_archive::Execution {
                    id: "x1".to_string(),
                    call_id: "c1".to_string(),
                    tool: "shell".to_string(),
                    arguments: r#"{"command":"ls"}"#.to_string(),
                    stage_index: 0,
                    iteration: 1,
                    dispatched_at: 100,
                    position: 6,
                    ended_at: Some(101),
                    result_position: Some(6),
                    outcome: Some(ToolOutcome::Succeeded),
                },
            };
            let schema =
                Schema::build(OneExecution { execution }, EmptyMutation, EmptySubscription)
                    .data(state_with_agent_paths(Vec::new()))
                    .finish();
            let answer = schema
                .execute(Request::new("{ execution { result { text } } }"))
                .await;
            let message = answer
                .errors
                .first()
                .map(|e| e.message.clone())
                .expect("a failure");
            assert!(message.contains("unreadable journal"), "{message}");
        },
    )
    .await;
}

/// A root handing out one execution directly.
struct OneExecution {
    execution: super::execution::ToolExecution,
}

#[async_graphql::Object]
impl OneExecution {
    /// The execution under test.
    async fn execution(&self) -> &super::execution::ToolExecution {
        &self.execution
    }
}
