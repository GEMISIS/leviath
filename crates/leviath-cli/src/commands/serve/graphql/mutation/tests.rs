//! Tests for the write side.
//!
//! Whole mutations run against the schema over an isolated runs directory and
//! a fake daemon, so what is asserted is what a client reads back: the entity
//! after the act, or an error carrying the code to branch on.
//!
//! Every field takes one argument named `request`, so the queries here are all
//! the same shape. The parts worth reading are the sweeps: a bulk act over a
//! filter answers with what moved and what it passed over, and each reason is
//! reached through a run in that state rather than asserted on the mapping
//! function alone.

use async_graphql::{EmptySubscription, Request, Schema};
use leviath_runtime::control_socket::{ControlClient, ControlResponse};

use super::blueprints::{CreateBlueprintRequest, DeleteBlueprintRequest, UpdateBlueprintRequest};
use super::catalog::RefreshModelsRequest;
use super::exports::StartRunExportRequest;
use super::interactions::{
    AnswerInteractionRequest, ApproveWrite, DenyWrite, InteractionAnswerWrite,
};
use super::runs::{
    CallbackWrite, DeleteRunsRequest, OutputRequestWrite, PauseRunRequest, PauseRunsRequest,
    RegionSeedWrite, SendMessageRequest, SpawnRunRequest, YoloWrite,
};
use super::{Mutation, has_landed, settle};
use crate::commands::serve::graphql::inputs::{BlueprintRef, KeyValueWrite, RegionRef};
use crate::commands::serve::graphql::mutation::attachments::{AttachmentWrite, Delivery};
use crate::commands::serve::graphql::query::Query;
use crate::commands::serve::graphql::types::interaction::ApprovalScope;
use crate::commands::serve::testutil::{fake_daemon, no_daemon_client, state_with_agent_paths};
use crate::runstate::{RunMeta, RunStatus, create_run};

/// A run on disk in the given state.
fn run_in(id: &str, status: RunStatus) -> RunMeta {
    let mut meta = RunMeta::new(
        id.to_string(),
        "test-agent".to_string(),
        "/agents/test".to_string(),
        "do the thing".to_string(),
        None,
        "/work".to_string(),
        1,
    );
    meta.status = status;
    meta
}

/// A run directory whose `meta.json` will not parse.
///
/// Such a run is invisible to every listing and cannot be shown to be
/// finished, which is the state the delete's `force` exists for.
fn unreadable_run(id: &str) {
    let dir = crate::runstate::run_dir(id);
    std::fs::create_dir_all(&dir).expect("the run directory");
    std::fs::write(dir.join(leviath_core::files::META_FILE), "{not json")
        .expect("the broken record");
}

/// An empty agents directory, so no test reads the developer's own.
fn empty_agents() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temp agents dir")
}

/// A temporary agents directory holding one blueprint by that name.
///
/// A spawn checks the blueprint exists before it reaches the daemon, and with
/// no path configured that check reads the developer's own agents directory. A
/// test that passed only on a machine with `coder` installed is a test that
/// says nothing, so every spawn test brings its own.
fn agents_dir_with(name: &str) -> tempfile::TempDir {
    let agents = tempfile::tempdir().expect("a temp dir");
    let agent = agents.path().join(name);
    std::fs::create_dir_all(&agent).expect("the agent dir");
    std::fs::write(
        agent.join(leviath_core::files::MANIFEST_FILENAME),
        format!(
            "[agent]\nname = \"{name}\"\n\n[context.regions.plan]\nkind = \"pinned\"\n\
             max_tokens = 100\n\n[stages.only]\nmode = \"autonomous\"\n"
        ),
    )
    .expect("manifest written");
    agents
}

/// Run one mutation against a schema wired to `control` and an agents
/// directory.
async fn mutate_with_agents(
    control: ControlClient,
    agents: &std::path::Path,
    query: &str,
) -> async_graphql::Response {
    let mut state = state_with_agent_paths(vec![agents.to_path_buf()]);
    state.control = control;
    let schema = Schema::build(Query, Mutation::default(), EmptySubscription)
        .data(state)
        .finish();
    schema.execute(Request::new(query)).await
}

/// Run one mutation against a schema wired to the given daemon.
async fn mutate(control: ControlClient, query: &str) -> async_graphql::Response {
    let agents = empty_agents();
    mutate_with_agents(control, agents.path(), query).await
}

/// A fake daemon that answers every request it is asked, not only the first.
///
/// The shared helper answers one, which is what a REST handler makes. A bulk
/// act makes one control request per run it reaches, so a sweep needs a daemon
/// that keeps answering.
fn busy_daemon(
    respond: impl Fn(leviath_runtime::control_socket::ControlRequest) -> ControlResponse
    + Send
    + Sync
    + 'static,
) -> (
    ControlClient,
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
) {
    use leviath_runtime::control_socket::{ControlRequest, bind_control_listener, control_id};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let dir = tempfile::tempdir().expect("a temp socket dir");
    let id = control_id(dir.path());
    let mut listener = bind_control_listener(&id).expect("the listener binds");
    let respond = std::sync::Arc::new(respond);
    let handle = tokio::spawn(async move {
        while let Ok(Some(stream)) = listener.accept().await {
            let respond = std::sync::Arc::clone(&respond);
            tokio::spawn(async move {
                let (read_half, mut write_half) = tokio::io::split(stream);
                let mut lines = BufReader::new(read_half).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(request) = serde_json::from_str::<ControlRequest>(&line) else {
                        return;
                    };
                    let mut out =
                        serde_json::to_string(&respond(request)).expect("the reply serializes");
                    out.push('\n');
                    let _ = write_half.write_all(out.as_bytes()).await;
                }
            });
        }
    });
    (ControlClient::new(id), dir, handle)
}

/// The `extensions.code` a response's first error carries.
fn code_of(answer: &async_graphql::Response) -> String {
    answer
        .errors
        .first()
        .expect("a refusal")
        .extensions
        .as_ref()
        .and_then(|extensions| extensions.get("code"))
        .map(ToString::to_string)
        .unwrap_or_default()
}

/// A response's data as JSON.
fn data_of(answer: &async_graphql::Response) -> serde_json::Value {
    serde_json::to_value(&answer.data).expect("data serializes")
}

// ── the single-run acts ──

/// The three acts, each answering with the run as it is afterwards.
///
/// The fake daemon says yes without changing anything, so the status here is
/// the record's; what this checks is that the run comes back at all, which is
/// what saves a client a second request.
#[tokio::test]
async fn each_act_answers_with_the_run() {
    crate::runstate::with_isolated_runs_dir_async("graphql-mutation-acts", |_d| async move {
        create_run(&run_in("run-a", RunStatus::Running)).expect("run written");

        for field in ["pauseRun", "resumeRun", "cancelRun"] {
            let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: true });
            let answer = mutate(
                control,
                &format!(
                    "mutation {{ {field}(request: {{ id: \"run-a\" }}) \
                     {{ run {{ id status }} warnings }} }}"
                ),
            )
            .await;
            assert!(answer.errors.is_empty(), "{field}: {:?}", answer.errors);
            let json = data_of(&answer);
            assert_eq!(json[field]["run"]["id"], "run-a", "{field}");
            assert_eq!(json[field]["run"]["status"], "RUNNING", "{field}");
            assert_eq!(
                json[field]["warnings"].as_array().map(Vec::len),
                Some(0),
                "{field}"
            );
        }
    })
    .await;
}

/// A finished run is a conflict, with the code a client branches on, rather
/// than a request that quietly does nothing.
#[tokio::test]
async fn a_finished_run_is_refused_with_a_conflict() {
    crate::runstate::with_isolated_runs_dir_async("graphql-mutation-done", |_d| async move {
        create_run(&run_in("run-done", RunStatus::Complete)).expect("run written");

        let answer = mutate(
            no_daemon_client(),
            "mutation { pauseRun(request: { id: \"run-done\" }) { run { id } } }",
        )
        .await;
        let error = answer.errors.first().expect("a refusal");
        assert!(error.message.contains("has finished"), "{}", error.message);
        assert_eq!(code_of(&answer), "\"CONFLICT\"");
        assert_eq!(
            error
                .extensions
                .as_ref()
                .and_then(|extensions| extensions.get("httpStatus"))
                .map(ToString::to_string),
            Some("409".to_string())
        );
    })
    .await;
}

/// A run the daemon does not know is a `NOT_FOUND`, and the message names both
/// things the daemon's one "no" can mean.
#[tokio::test]
async fn a_run_the_daemon_refuses_is_not_found() {
    crate::runstate::with_isolated_runs_dir_async("graphql-mutation-ghost", |_d| async move {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: false });
        let answer = mutate(
            control,
            "mutation { resumeRun(request: { id: \"ghost\" }) { run { id } } }",
        )
        .await;
        let error = answer.errors.first().expect("a refusal");
        assert!(error.message.contains("not paused"), "{}", error.message);
        assert_eq!(code_of(&answer), "\"NOT_FOUND\"");
    })
    .await;
}

/// A daemon that cannot be reached is its own failure, because its remedy is
/// its own: get the daemon back, then ask again.
#[tokio::test]
async fn an_unreachable_daemon_is_told_apart_from_a_missing_run() {
    crate::runstate::with_isolated_runs_dir_async("graphql-mutation-nodaemon", |_d| async move {
        create_run(&run_in("run-a", RunStatus::Running)).expect("run written");
        let answer = mutate(
            no_daemon_client(),
            "mutation { cancelRun(request: { id: \"run-a\" }) { run { id } } }",
        )
        .await;
        assert_eq!(code_of(&answer), "\"DAEMON_UNAVAILABLE\"");
    })
    .await;
}

/// A record that will not read after the act is this server's problem, and the
/// message says so rather than blaming the caller.
#[tokio::test]
async fn a_record_that_will_not_read_after_the_act_is_internal() {
    crate::runstate::with_isolated_runs_dir_async("graphql-mutation-unread", |_d| async move {
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: true });
        let answer = mutate(
            control,
            "mutation { pauseRun(request: { id: \"never-written\" }) { run { id } } }",
        )
        .await;
        assert_eq!(code_of(&answer), "\"INTERNAL\"");
        assert!(
            answer.errors[0].message.contains("would not read"),
            "{}",
            answer.errors[0].message
        );
    })
    .await;
}

// ── the bulk acts ──

/// A sweep moves what it can and says what it passed over, with a reason per
/// run rather than one failure for the set.
///
/// The four reasons are the four states a run can be in when a sweep reaches
/// it, so they are all here at once: one going, one already finished, one whose
/// record will not read, and one the daemon has never heard of.
#[tokio::test]
async fn a_sweep_moves_what_it_can_and_names_what_it_did_not() {
    crate::runstate::with_isolated_runs_dir_async("graphql-sweep", |_d| async move {
        create_run(&run_in("going", RunStatus::Running)).expect("run written");
        create_run(&run_in("finished", RunStatus::Complete)).expect("run written");
        unreadable_run("broken");

        let (control, _dir, _srv) = busy_daemon(|request| match request {
            leviath_runtime::control_socket::ControlRequest::Pause { run_id } => {
                // The daemon knows the runs it was told about and nothing
                // about the id that names no run at all.
                ControlResponse::Ok {
                    ok: run_id != "ghost",
                }
            }
            other => panic!("a pause, not {other:?}"),
        });
        let answer = mutate(
            control,
            r#"mutation { pauseRuns(request: { filter: { id: {
                 in: ["going", "finished", "broken", "ghost"] } } })
                 { runs { id } skipped { id reason message } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = data_of(&answer);
        assert_eq!(
            json["pauseRuns"]["runs"],
            serde_json::json!([{"id": "going"}])
        );
        let skipped = json["pauseRuns"]["skipped"]
            .as_array()
            .expect("a skipped list");
        let reasons: Vec<(&str, &str)> = skipped
            .iter()
            .map(|entry| {
                (
                    entry["id"].as_str().unwrap_or_default(),
                    entry["reason"].as_str().unwrap_or_default(),
                )
            })
            .collect();
        assert!(
            reasons.contains(&("finished", "ALREADY_FINISHED")),
            "{reasons:?}"
        );
        assert!(
            reasons.contains(&("broken", "RECORD_UNREADABLE")),
            "{reasons:?}"
        );
        assert!(reasons.contains(&("ghost", "OTHER")), "{reasons:?}");
        // Each skip says why in words as well as in the enum.
        assert!(
            skipped
                .iter()
                .all(|entry| !entry["message"].as_str().unwrap_or_default().is_empty()),
            "{skipped:?}"
        );
    })
    .await;
}

/// A run that finishes while the sweep's own act is in flight is passed over,
/// never listed as one the sweep moved.
///
/// The daemon applies a cancel to its world and the persistence lane writes the
/// record a tick later, so the record read before the act says `running` for a
/// run that is already over. The daemon's cancel is unconditional once it gets
/// there: it forces such a run onto `cancelled` on disk, which is a finished
/// run's own answer overwritten and a `RunCompletedEvent` turned into a lie.
/// The record is read again after the act, and a finish the act did not produce
/// is the conflict it always was.
#[tokio::test]
async fn a_run_that_finishes_under_a_sweep_is_skipped_rather_than_overwritten() {
    crate::runstate::with_isolated_runs_dir_async("graphql-sweep-race", |_d| async move {
        create_run(&run_in("racing", RunStatus::Running)).expect("run written");

        let (control, _dir, _srv) = busy_daemon(|request| match request {
            leviath_runtime::control_socket::ControlRequest::Cancel { .. } => {
                // The run reached its own end while this request was on its
                // way, and the lane wrote the record just before the daemon
                // answered.
                crate::runstate::write_meta(&run_in("racing", RunStatus::Complete))
                    .expect("the finish is written");
                ControlResponse::Ok { ok: true }
            }
            other => panic!("a cancel, not {other:?}"),
        });
        let answer = mutate(
            control,
            r#"mutation { cancelRuns(request: { filter: { id: { in: ["racing"] } } })
                 { runs { id status } skipped { id reason } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = data_of(&answer);
        assert_eq!(
            json["cancelRuns"]["runs"],
            serde_json::json!([]),
            "a run that finished by itself is not a run the sweep moved"
        );
        assert_eq!(
            json["cancelRuns"]["skipped"],
            serde_json::json!([{"id": "racing", "reason": "ALREADY_FINISHED"}])
        );
        // And what the run says about itself is still its own answer.
        assert_eq!(
            crate::runstate::read_meta("racing")
                .expect("the record")
                .status,
            RunStatus::Complete
        );
    })
    .await;
}

/// The same race against the single-run act is the conflict a client can tell
/// "you stopped it" from "it was over before you asked" by.
#[tokio::test]
async fn a_run_that_finishes_under_a_cancel_is_a_conflict() {
    crate::runstate::with_isolated_runs_dir_async("graphql-cancel-race", |_d| async move {
        create_run(&run_in("racing", RunStatus::Running)).expect("run written");
        let (control, _dir, _srv) = busy_daemon(|_| {
            crate::runstate::write_meta(&run_in("racing", RunStatus::Complete))
                .expect("the finish is written");
            ControlResponse::Ok { ok: true }
        });
        let answer = mutate(
            control,
            r#"mutation { cancelRun(request: { id: "racing" }) { run { status } } }"#,
        )
        .await;
        assert_eq!(code_of(&answer), "\"CONFLICT\"");
    })
    .await;
}

/// A sweep answers with each run as the act left it, not as it stood before.
///
/// The daemon moves the run in its world and answers; the record is written a
/// tick later. A sweep that read the record the moment the daemon replied
/// answered `RUNNING` for every run it had just paused, which is the one thing
/// the field is there to say.
#[tokio::test]
async fn a_sweep_answers_with_the_status_the_act_left() {
    crate::runstate::with_isolated_runs_dir_async("graphql-sweep-settle", |_d| async move {
        for id in ["one", "two"] {
            create_run(&run_in(id, RunStatus::Running)).expect("run written");
        }
        let (control, _dir, _srv) = busy_daemon(|request| match request {
            leviath_runtime::control_socket::ControlRequest::Pause { run_id } => {
                // The lane writes the record after the daemon has answered,
                // which is the ordering the settle loop exists for.
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                    crate::runstate::write_meta(&run_in(&run_id, RunStatus::Paused))
                        .expect("the pause is written");
                });
                ControlResponse::Ok { ok: true }
            }
            other => panic!("a pause, not {other:?}"),
        });
        let answer = mutate(
            control,
            r#"mutation { pauseRuns(request: { filter: { id: { in: ["one", "two"] } } })
                 { runs { id status } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = data_of(&answer);
        // The sweep's own order is the listing's, newest first, so what is
        // asserted is the pair rather than which came back first.
        let mut moved: Vec<(&str, &str)> = json["pauseRuns"]["runs"]
            .as_array()
            .expect("the runs that moved")
            .iter()
            .map(|run| {
                (
                    run["id"].as_str().unwrap_or_default(),
                    run["status"].as_str().unwrap_or_default(),
                )
            })
            .collect();
        moved.sort_unstable();
        assert_eq!(moved, vec![("one", "PAUSED"), ("two", "PAUSED")]);
    })
    .await;
}

/// The other two sweeps do the same thing with their own verb, and answer with
/// their own result type.
#[tokio::test]
async fn resuming_and_cancelling_sweep_the_same_way() {
    crate::runstate::with_isolated_runs_dir_async("graphql-sweep-verbs", |_d| async move {
        create_run(&run_in("going", RunStatus::Paused)).expect("run written");

        for field in ["resumeRuns", "cancelRuns"] {
            let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: true });
            let answer = mutate(
                control,
                &format!(
                    "mutation {{ {field}(request: {{ filter: {{ id: {{ in: [\"going\"] }} }} }}) \
                     {{ runs {{ id }} skipped {{ id }} }} }}"
                ),
            )
            .await;
            assert!(answer.errors.is_empty(), "{field}: {:?}", answer.errors);
            let json = data_of(&answer);
            assert_eq!(json[field]["runs"][0]["id"], "going", "{field}");
        }
    })
    .await;
}

/// A daemon that is not there is not a fact about one run, so it ends the whole
/// sweep rather than landing in `skipped`.
#[tokio::test]
async fn an_unreachable_daemon_ends_a_sweep() {
    crate::runstate::with_isolated_runs_dir_async("graphql-sweep-nodaemon", |_d| async move {
        create_run(&run_in("going", RunStatus::Running)).expect("run written");
        let answer = mutate(
            no_daemon_client(),
            r#"mutation { cancelRuns(request: { filter: { id: { in: ["going"] } } })
                 { runs { id } } }"#,
        )
        .await;
        assert_eq!(code_of(&answer), "\"DAEMON_UNAVAILABLE\"");
    })
    .await;
}

/// An empty filter names every run on the machine, so every destructive sweep
/// refuses it rather than acting on all of them.
#[tokio::test]
async fn a_sweep_over_an_empty_filter_is_refused() {
    crate::runstate::with_isolated_runs_dir_async("graphql-sweep-empty", |_d| async move {
        create_run(&run_in("going", RunStatus::Running)).expect("run written");
        let cases = [
            ("pauseRuns", "pause", "{}"),
            ("resumeRuns", "resume", "{}"),
            ("cancelRuns", "cancel", "{}"),
            // A filter of nothing but nulls is the same empty filter: what was
            // sent is read rather than which fields were named.
            ("deleteRuns", "delete", "{ title: null }"),
        ];
        for (field, verb, filter) in cases {
            let answer = mutate(
                no_daemon_client(),
                &format!(
                    "mutation {{ {field}(request: {{ filter: {filter} }}) \
                     {{ skipped {{ id }} }} }}"
                ),
            )
            .await;
            assert_eq!(code_of(&answer), "\"BAD_USER_INPUT\"", "{field}");
            assert!(
                answer.errors[0].message.contains(verb),
                "it names the act: {}",
                answer.errors[0].message
            );
        }
    })
    .await;
}

// ── deleting ──

/// A delete removes a run and its sub-agents, and says what it removed.
#[tokio::test]
async fn a_delete_takes_a_runs_sub_agents_with_it() {
    crate::runstate::with_isolated_runs_dir_async("graphql-delete", |_d| async move {
        create_run(&run_in("root", RunStatus::Complete)).expect("run written");
        let mut worker = run_in("worker", RunStatus::Complete);
        worker.parent_run_id = Some("root".to_string());
        create_run(&worker).expect("run written");

        let answer = mutate(
            no_daemon_client(),
            r#"mutation { deleteRuns(request: { filter: { id: { in: ["root"] } } })
                 { deletedIds skipped { id reason } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = data_of(&answer);
        let deleted = json["deleteRuns"]["deletedIds"]
            .as_array()
            .expect("deleted ids");
        assert_eq!(deleted.len(), 2, "the run and its worker: {deleted:?}");
        assert_eq!(
            json["deleteRuns"]["skipped"].as_array().map(Vec::len),
            Some(0)
        );
        assert!(
            !crate::runstate::run_dir("root").exists(),
            "the record is gone"
        );
    })
    .await;
}

/// Each reason a delete passes a run over, reached through a run in that
/// state: one still going, one whose record will not read, one that is not
/// there at all.
#[tokio::test]
async fn a_delete_names_why_it_left_each_run() {
    crate::runstate::with_isolated_runs_dir_async("graphql-delete-skips", |_d| async move {
        create_run(&run_in("still-going", RunStatus::Running)).expect("run written");
        unreadable_run("broken");

        let answer = mutate(
            no_daemon_client(),
            r#"mutation { deleteRuns(request: { filter: {
                 id: { in: ["still-going", "broken", "ghost"] } } })
                 { deletedIds skipped { id reason message } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = data_of(&answer);
        assert_eq!(
            json["deleteRuns"]["deletedIds"].as_array().map(Vec::len),
            Some(0)
        );
        let reasons: Vec<(&str, &str)> = json["deleteRuns"]["skipped"]
            .as_array()
            .expect("a skipped list")
            .iter()
            .map(|entry| {
                (
                    entry["id"].as_str().unwrap_or_default(),
                    entry["reason"].as_str().unwrap_or_default(),
                )
            })
            .collect();
        assert!(
            reasons.contains(&("still-going", "STILL_RUNNING")),
            "{reasons:?}"
        );
        assert!(
            reasons.contains(&("broken", "RECORD_UNREADABLE")),
            "{reasons:?}"
        );
        assert!(reasons.contains(&("ghost", "OTHER")), "{reasons:?}");
    })
    .await;
}

/// `force` is what deletes a run nothing can be shown about, and it is the
/// caller's word rather than something that happens to them.
#[tokio::test]
async fn force_removes_a_run_whose_record_will_not_read() {
    crate::runstate::with_isolated_runs_dir_async("graphql-delete-force", |_d| async move {
        unreadable_run("broken");
        let answer = mutate(
            no_daemon_client(),
            r#"mutation { deleteRuns(request: { filter: { id: { in: ["broken"] } }, force: true })
                 { deletedIds skipped { id } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = data_of(&answer);
        assert_eq!(
            json["deleteRuns"]["deletedIds"],
            serde_json::json!(["broken"])
        );
        assert!(!crate::runstate::run_dir("broken").exists());
    })
    .await;
}

/// A sweep by age is the run filter's own comparison rather than an argument of
/// its own, and it takes the finished runs it names.
#[tokio::test]
async fn a_delete_sweeps_by_the_filter_the_listing_uses() {
    crate::runstate::with_isolated_runs_dir_async("graphql-delete-sweep", |_d| async move {
        let mut old = run_in("old", RunStatus::Complete);
        old.updated_at = 100;
        create_run(&old).expect("run written");
        let mut recent = run_in("recent", RunStatus::Complete);
        recent.updated_at = 5_000;
        create_run(&recent).expect("run written");

        let answer = mutate(
            no_daemon_client(),
            "mutation { deleteRuns(request: { filter: { updatedAt: { lt: 1000 } } }) \
             { deletedIds } }",
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = data_of(&answer);
        assert_eq!(json["deleteRuns"]["deletedIds"], serde_json::json!(["old"]));
    })
    .await;
}

/// Naming ids outright says which runs to read, not which ones to act on: the
/// rest of the filter still decides, exactly as it does on the listing.
///
/// The one id a named list adds on its own is a run the index has never seen,
/// whose record will not parse: it is in no listing, so nothing else could ever
/// name it, and `force` is what then deletes it.
#[tokio::test]
async fn naming_ids_does_not_excuse_a_run_from_the_rest_of_the_filter() {
    crate::runstate::with_isolated_runs_dir_async("graphql-sweep-named-ids", |_d| async move {
        create_run(&run_in("going", RunStatus::Running)).expect("run written");
        create_run(&run_in("parked", RunStatus::Paused)).expect("run written");
        unreadable_run("broken");

        let (control, _dir, _srv) = busy_daemon(|_| ControlResponse::Ok { ok: true });
        let answer = mutate(
            control,
            r#"mutation { cancelRuns(request: { filter: {
                 id: { in: ["going", "parked", "broken"] }, status: { eq: RUNNING } } })
                 { runs { id } skipped { id reason } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = data_of(&answer);
        assert_eq!(
            json["cancelRuns"]["runs"],
            serde_json::json!([{"id": "going"}]),
            "a paused run is not a RUNNING run"
        );
        let touched: Vec<&str> = json["cancelRuns"]["skipped"]
            .as_array()
            .expect("a skipped list")
            .iter()
            .map(|entry| entry["id"].as_str().unwrap_or_default())
            .collect();
        assert!(
            !touched.contains(&"parked"),
            "a run the filter excluded is not acted on at all: {touched:?}"
        );
        // The unreadable record is still reachable, because no listing can
        // name it and dropping it here would leave it undeletable.
        assert!(touched.contains(&"broken"), "{touched:?}");
    })
    .await;
}

/// A filter that contradicts itself names nothing rather than everything it
/// mentioned.
#[tokio::test]
async fn ids_named_twice_over_are_intersected_not_added_up() {
    crate::runstate::with_isolated_runs_dir_async("graphql-sweep-contradiction", |_d| async move {
        create_run(&run_in("one", RunStatus::Complete)).expect("run written");
        create_run(&run_in("two", RunStatus::Complete)).expect("run written");

        let answer = mutate(
            no_daemon_client(),
            r#"mutation { deleteRuns(request: { filter: {
                 id: { eq: "one", in: ["two"] } } })
                 { deletedIds skipped { id } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = data_of(&answer);
        assert_eq!(
            json["deleteRuns"]["deletedIds"],
            serde_json::json!([]),
            "no run is both `one` and `two`"
        );
        assert!(crate::runstate::run_dir("one").exists());
        assert!(crate::runstate::run_dir("two").exists());
    })
    .await;
}

/// The refusals a sweep shares with the listing it takes its filter from.
#[tokio::test]
async fn a_sweep_refuses_what_the_listing_refuses() {
    crate::runstate::with_isolated_runs_dir_async("graphql-sweep-refusals", |_d| async move {
        // A filter too large to walk: the listing refuses it before comparing a
        // single run, and so does the sweep that takes the same filter.
        let deep = (0..18).fold("{ task: { eq: \"x\" } }".to_string(), |inner, _| {
            format!("{{ not: {inner} }}")
        });
        let refused = mutate(
            no_daemon_client(),
            &format!("mutation {{ deleteRuns(request: {{ filter: {deep} }}) {{ deletedIds }} }}"),
        )
        .await;
        assert_eq!(code_of(&refused), "\"BAD_USER_INPUT\"");
        assert!(
            refused.errors[0].message.contains("split the query"),
            "it says what to do about it: {}",
            refused.errors[0].message
        );

        // And more ids than one request may name: the cap is the listing's,
        // and deleting is the act where exceeding it matters most.
        let many: Vec<String> = (0..300).map(|i| format!("\"run-{i}\"")).collect();
        let too_many = mutate(
            no_daemon_client(),
            &format!(
                "mutation {{ deleteRuns(request: {{ filter: {{ id: {{ in: [{}] }} }} }}) \
                 {{ deletedIds }} }}",
                many.join(", ")
            ),
        )
        .await;
        assert!(
            too_many.errors[0].message.contains("at most"),
            "it says what the cap is: {}",
            too_many.errors[0].message
        );
    })
    .await;
}

// ── spawning ──

/// A spawn answers with the run it started, and carries every field it was
/// given down to the daemon.
///
/// The fake daemon records what it was sent, so this asserts the translation as
/// well as the answer: a field a client sets and the daemon never sees is a
/// field that silently does nothing.
#[tokio::test]
async fn a_spawn_carries_every_field_it_was_given() {
    crate::runstate::with_isolated_runs_dir_async("graphql-spawn", |_d| async move {
        let agents = agents_dir_with("coder");
        let workdir = tempfile::tempdir().expect("a temp workdir");
        std::fs::write(workdir.path().join("hero.png"), b"\x89PNG\r\n\x1a\nbody")
            .expect("the attachment");
        let (control, _dir, _srv) = fake_daemon(|request| match request {
            leviath_runtime::control_socket::ControlRequest::Spawn { args } => {
                assert!(
                    args.blueprint_path.contains("coder"),
                    "the blueprint it named: {}",
                    args.blueprint_path
                );
                assert_eq!(args.task, "fix the parser");
                assert_eq!(args.model.as_deref(), Some("gpt-5.6"));
                assert_eq!(args.max_depth, Some(3));
                assert!(args.yolo, "the waiver travels");
                assert_eq!(
                    args.regions.get("plan").map(String::as_str),
                    Some("start here")
                );
                assert_eq!(args.metadata.get("ticket").map(String::as_str), Some("42"));
                assert_eq!(args.parts.len(), 1, "the attachment travels");
                assert_eq!(args.parts[0].name, "the-hero");
                assert_eq!(args.parts[0].region.as_deref(), Some("plan"));
                assert_eq!(
                    args.parts[0].deliver,
                    Some(leviath_core::mime::Delivery::Text)
                );
                assert_eq!(args.parts[0].caption.as_deref(), Some("v1"));
                assert_eq!(
                    args.parts[0].mime_type.as_ref().map(|t| t.as_str()),
                    Some("image/png")
                );
                let mut meta = run_in(&args.run_id, RunStatus::Starting);
                meta.task = args.task.clone();
                meta.metadata = args.metadata.clone();
                create_run(&meta).expect("run written");
                ControlResponse::Spawned {
                    run_id: args.run_id,
                }
            }
            other => panic!("the spawn is what reaches the daemon, not {other:?}"),
        });
        let mut state = state_with_agent_paths(vec![agents.path().to_path_buf()]);
        state.control = control;
        let schema = Schema::build(Query, Mutation::default(), EmptySubscription)
            .data(state)
            .finish();
        // The workdir is a variable rather than query text: a Windows path
        // holds backslashes, which a GraphQL string literal would eat.
        let answer = schema
            .execute(
                Request::new(
                    "mutation Start($request: SpawnRunRequest!) { \
                       spawnRun(request: $request) { run { id task status metadata { key value } } \
                       warnings } }",
                )
                .variables(async_graphql::Variables::from_json(
                    serde_json::json!({
                        "request": {
                            "blueprint": { "name": "coder" },
                            "task": "fix the parser",
                            "model": "gpt-5.6",
                            "maxDepth": 3,
                            "workdir": workdir.path().to_string_lossy(),
                            "yolo": { "everything": true },
                            "allowTools": ["shell"],
                            "skipSeedCommands": true,
                            "captureModelInput": true,
                            "regions": [{ "region": { "name": "plan" }, "text": "start here" }],
                            "metadata": [{ "key": "ticket", "value": "42" }],
                            "output": { "format": "json", "instructions": "one object" },
                            "callback": { "url": "https://example.com/hook", "secret": "shh" },
                            "attachments": [{
                                "path": "hero.png",
                                "region": { "name": "plan" },
                                "name": "the-hero",
                                "mimeType": "image/png",
                                "deliver": "TEXT",
                                "caption": "v1",
                            }],
                        }
                    }),
                )),
            )
            .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = data_of(&answer);
        let run = &json["spawnRun"]["run"];
        assert_eq!(run["task"], "fix the parser");
        assert_eq!(run["status"], "STARTING");
        assert_eq!(run["metadata"][0]["key"], "ticket");
        assert_eq!(
            json["spawnRun"]["warnings"].as_array().map(Vec::len),
            Some(0)
        );
    })
    .await;
}

/// A spawn that says almost nothing: no yolo, no callback, no output shape, no
/// attachments. Every one of those is a branch of its own.
#[tokio::test]
async fn a_spawn_with_nothing_optional_set_still_starts() {
    crate::runstate::with_isolated_runs_dir_async("graphql-spawn-bare", |_d| async move {
        let agents = agents_dir_with("coder");
        let (control, _dir, _srv) = fake_daemon(|request| match request {
            leviath_runtime::control_socket::ControlRequest::Spawn { args } => {
                assert!(!args.yolo, "nothing was waived");
                assert!(args.yolo_profile.is_none());
                assert!(args.parts.is_empty());
                create_run(&run_in(&args.run_id, RunStatus::Starting)).expect("run written");
                ControlResponse::Spawned {
                    run_id: args.run_id,
                }
            }
            other => panic!("a spawn, not {other:?}"),
        });
        let answer = mutate_with_agents(
            control,
            agents.path(),
            r#"mutation { spawnRun(request: { blueprint: { name: "coder" }, task: "t" })
                 { run { id } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    })
    .await;
}

/// A named yolo profile is the other half of the waiver, and it travels as
/// itself rather than as a blanket one.
#[tokio::test]
async fn a_named_yolo_profile_travels_as_the_name() {
    crate::runstate::with_isolated_runs_dir_async("graphql-spawn-profile", |_d| async move {
        let agents = agents_dir_with("coder");
        let (control, _dir, _srv) = fake_daemon(|request| match request {
            leviath_runtime::control_socket::ControlRequest::Spawn { args } => {
                assert_eq!(args.yolo_profile.as_deref(), Some("cautious"));
                create_run(&run_in(&args.run_id, RunStatus::Starting)).expect("run written");
                ControlResponse::Spawned {
                    run_id: args.run_id,
                }
            }
            other => panic!("a spawn, not {other:?}"),
        });
        let answer = mutate_with_agents(
            control,
            agents.path(),
            r#"mutation { spawnRun(request: { blueprint: { name: "coder" }, task: "t",
                 yolo: { profileName: "cautious" } }) { run { id } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    })
    .await;
}

/// A spawn pinned to a revision that is not the installed one is a conflict,
/// and nothing is started.
#[tokio::test]
async fn a_spawn_pinned_to_another_revision_is_a_conflict() {
    crate::runstate::with_isolated_runs_dir_async("graphql-spawn-pinned", |_d| async move {
        let agents = agents_dir_with("coder");
        let answer = mutate_with_agents(
            no_daemon_client(),
            agents.path(),
            r#"mutation { spawnRun(request: {
                 blueprint: { name: "coder",
                   digest: "0000000000000000000000000000000000000000000000000000000000000000" },
                 task: "t" }) { run { id } } }"#,
        )
        .await;
        assert_eq!(code_of(&answer), "\"CONFLICT\"");
        assert!(
            answer.errors[0].message.contains("coder"),
            "{}",
            answer.errors[0].message
        );
    })
    .await;
}

/// The spawn's own refusals: a waiver this server does not allow, a negative
/// depth, and an attachment outside the working directory.
#[tokio::test]
async fn a_spawn_refuses_what_the_server_will_not_do() {
    crate::runstate::with_isolated_runs_dir_async("graphql-spawn-refused", |_d| async move {
        let agents = agents_dir_with("coder");
        let workdir = tempfile::tempdir().expect("a temp workdir");
        let mut state = state_with_agent_paths(vec![agents.path().to_path_buf()]);
        state.limits = std::sync::Arc::new(crate::commands::serve::types::ServeLimits {
            no_remote_yolo: true,
            ..Default::default()
        });
        let schema = Schema::build(Query, Mutation::default(), EmptySubscription)
            .data(state)
            .finish();

        let waived = schema
            .execute(
                Request::new(
                    "mutation Start($workdir: String!) { spawnRun(request: { \
                       blueprint: { name: \"coder\" }, task: \"t\", workdir: $workdir, \
                       yolo: { everything: true } }) { run { id } } }",
                )
                .variables(async_graphql::Variables::from_json(
                    serde_json::json!({
                        "workdir": workdir.path().to_string_lossy(),
                    }),
                )),
            )
            .await;
        assert_eq!(code_of(&waived), "\"FORBIDDEN\"");

        let negative = schema
            .execute(Request::new(
                r#"mutation { spawnRun(request: { blueprint: { name: "coder" }, task: "t",
                     maxDepth: -1 }) { run { id } } }"#,
            ))
            .await;
        assert!(
            negative.errors[0].message.contains("negative"),
            "{:?}",
            negative.errors
        );

        // An attachment that climbs out of the working directory is the one
        // refusal the path reader owns, and it reaches GraphQL as its own code.
        let escaping = schema
            .execute(
                Request::new(
                    "mutation Start($workdir: String!) { spawnRun(request: { \
                       blueprint: { name: \"coder\" }, task: \"t\", workdir: $workdir, \
                       attachments: [{ path: \"../escape.png\" }] }) { run { id } } }",
                )
                .variables(async_graphql::Variables::from_json(
                    serde_json::json!({
                        "workdir": workdir.path().to_string_lossy(),
                    }),
                )),
            )
            .await;
        assert_eq!(code_of(&escaping), "\"FORBIDDEN\"");
        assert!(
            escaping.errors[0].message.contains("working directory"),
            "{}",
            escaping.errors[0].message
        );
    })
    .await;
}

/// A spawn the daemon accepted whose record will not read is this server's
/// problem, and the message says so rather than blaming the caller.
#[tokio::test]
async fn a_record_that_will_not_read_after_a_spawn_is_internal() {
    crate::runstate::with_isolated_runs_dir_async("graphql-spawn-unread", |_d| async move {
        let agents = agents_dir_with("coder");
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Spawned {
            run_id: "ghost".to_string(),
        });
        let answer = mutate_with_agents(
            control,
            agents.path(),
            r#"mutation { spawnRun(request: { blueprint: { name: "coder" }, task: "t" })
                 { run { id } } }"#,
        )
        .await;
        assert_eq!(
            code_of(&answer),
            "\"INTERNAL\"",
            "the daemon said yes, so the missing record is ours"
        );
    })
    .await;
}

// ── messaging ──

/// A message answers with the run, and carries the files it named beside the
/// words.
#[tokio::test]
async fn a_message_answers_with_the_run_it_reached() {
    crate::runstate::with_isolated_runs_dir_async("graphql-message", |_d| async move {
        let workdir = tempfile::tempdir().expect("a temp workdir");
        std::fs::write(workdir.path().join("notes.txt"), b"read this").expect("the attachment");
        let mut meta = run_in("run-a", RunStatus::WaitingInput);
        meta.workdir = workdir.path().to_string_lossy().to_string();
        create_run(&meta).expect("run written");

        let (control, _dir, _srv) = fake_daemon(|request| match request {
            leviath_runtime::control_socket::ControlRequest::Message {
                agent_id,
                content,
                target_region,
                parts,
            } => {
                assert_eq!(agent_id, "run-a");
                assert_eq!(content, "keep going");
                assert_eq!(target_region.as_deref(), Some("plan"));
                assert_eq!(parts.len(), 1);
                assert_eq!(parts[0].name, "notes.txt");
                ControlResponse::Ok { ok: true }
            }
            other => panic!("the message is what reaches the daemon: {other:?}"),
        });

        let answer = mutate(
            control,
            r#"mutation { sendMessage(request: { runId: "run-a", text: "keep going",
                 region: { name: "plan" }, attachments: [{ path: "notes.txt" }] })
                 { run { id status } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = data_of(&answer);
        assert_eq!(json["sendMessage"]["run"]["id"], "run-a");
    })
    .await;
}

/// A run that does not take messages says that, rather than reading as missing.
#[tokio::test]
async fn a_run_that_takes_no_messages_says_so() {
    crate::runstate::with_isolated_runs_dir_async("graphql-message-refused", |_d| async move {
        create_run(&run_in("run-a", RunStatus::Running)).expect("run written");
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: false });
        let answer = mutate(
            control,
            r#"mutation { sendMessage(request: { runId: "run-a", text: "hello" })
                 { run { id } } }"#,
        )
        .await;
        assert!(
            answer.errors[0].message.contains("not accepting messages"),
            "{}",
            answer.errors[0].message
        );
    })
    .await;
}

/// Attachments are named inside the run's own working directory, so a message
/// carrying some to a run this server cannot read is refused before the daemon
/// is asked.
#[tokio::test]
async fn a_message_with_files_needs_a_run_it_can_read() {
    crate::runstate::with_isolated_runs_dir_async("graphql-message-unread", |_d| async move {
        let answer = mutate(
            no_daemon_client(),
            r#"mutation { sendMessage(request: { runId: "ghost", text: "hi",
                 attachments: [{ path: "notes.txt" }] }) { run { id } } }"#,
        )
        .await;
        assert_eq!(code_of(&answer), "\"INTERNAL\"");
    })
    .await;
}

// ── answering an ask ──

/// Each answer variant reaches the daemon as what that kind of ask takes.
#[tokio::test]
async fn each_answer_variant_reaches_the_daemon() {
    let cases = [
        ("{ text: \"yes\" }", "text"),
        ("{ choice: 1 }", "choice"),
        ("{ approve: { scope: RUN } }", "approve"),
        ("{ deny: { feedback: \"read the file instead\" } }", "deny"),
    ];
    for (answer, kind) in cases {
        let (control, _socket, _srv) = fake_daemon(move |request| match request {
            leviath_runtime::control_socket::ControlRequest::AnswerInteraction { response } => {
                assert_eq!(response.request_id, "ask-1");
                match kind {
                    "text" => assert_eq!(response.value.as_deref(), Some("yes")),
                    "choice" => assert_eq!(response.choice_index, Some(1)),
                    "approve" => {
                        assert_eq!(response.approved, Some(true));
                        assert_eq!(
                            response.scope,
                            Some(leviath_core::interaction::ApprovalScope::Run)
                        );
                    }
                    _ => {
                        assert_eq!(response.approved, Some(false));
                        assert_eq!(response.feedback.as_deref(), Some("read the file instead"));
                        assert_eq!(
                            response.scope,
                            Some(leviath_core::interaction::ApprovalScope::Once),
                            "a denial covers the one call it was asked about"
                        );
                    }
                }
                ControlResponse::Ok { ok: true }
            }
            other => panic!("the answer is what reaches the daemon: {other:?}"),
        });
        let response = mutate(
            control,
            &format!(
                "mutation {{ answerInteraction(request: {{ interactionId: \"ask-1\", \
                 answer: {answer} }}) {{ interactionId outcome }} }}"
            ),
        )
        .await;
        assert!(response.errors.is_empty(), "{kind}: {:?}", response.errors);
        let json = data_of(&response);
        assert_eq!(json["answerInteraction"]["outcome"], "ACCEPTED", "{kind}");
        assert_eq!(
            json["answerInteraction"]["interactionId"], "ask-1",
            "{kind}"
        );
    }
}

/// An approval with nothing said about its scope covers the one call it was
/// given for: the narrowest grant is what a request that does not say wants.
#[tokio::test]
async fn an_approval_that_says_no_scope_covers_one_call() {
    let (control, _socket, _srv) = fake_daemon(|request| match request {
        leviath_runtime::control_socket::ControlRequest::AnswerInteraction { response } => {
            assert_eq!(
                response.scope,
                Some(leviath_core::interaction::ApprovalScope::Once)
            );
            ControlResponse::Ok { ok: true }
        }
        other => panic!("an answer, not {other:?}"),
    });
    let answer = mutate(
        control,
        r#"mutation { answerInteraction(request: { interactionId: "ask-1",
             answer: { approve: {} } }) { outcome } }"#,
    )
    .await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
}

/// Each scope is a different grant, and each reaches the daemon as itself.
#[tokio::test]
async fn each_approval_scope_reaches_the_daemon() {
    for (word, expected) in [
        ("ONCE", leviath_core::interaction::ApprovalScope::Once),
        ("STAGE", leviath_core::interaction::ApprovalScope::Stage),
        ("RUN", leviath_core::interaction::ApprovalScope::Run),
    ] {
        let (control, _dir, _srv) = fake_daemon(move |request| match request {
            leviath_runtime::control_socket::ControlRequest::AnswerInteraction { response } => {
                assert_eq!(response.scope, Some(expected));
                ControlResponse::Ok { ok: true }
            }
            other => panic!("an answer, not {other:?}"),
        });
        let answer = mutate(
            control,
            &format!(
                "mutation {{ answerInteraction(request: {{ interactionId: \"r1\", \
                 answer: {{ approve: {{ scope: {word} }} }} }}) {{ outcome }} }}"
            ),
        )
        .await;
        assert!(answer.errors.is_empty(), "{word}: {:?}", answer.errors);
    }
}

/// A second answer to one request is not an error: it reads as already
/// settled. Two people clicking the same prompt is ordinary, and the first one
/// won.
#[tokio::test]
async fn a_second_answer_reads_as_already_settled() {
    let (control, _socket, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: false });
    let answer = mutate(
        control,
        r#"mutation { answerInteraction(request: { interactionId: "ask-1",
             answer: { text: "yes" } }) { interactionId outcome } }"#,
    )
    .await;
    assert!(answer.errors.is_empty(), "{:?}", answer.errors);
    let json = data_of(&answer);
    assert_eq!(json["answerInteraction"]["outcome"], "ALREADY_SETTLED");

    // A daemon that cannot be reached is still a failure: nothing was
    // answered, and the remedy is not the client's.
    let unreachable = mutate(
        no_daemon_client(),
        r#"mutation { answerInteraction(request: { interactionId: "ask-1",
             answer: { text: "y" } }) { outcome } }"#,
    )
    .await;
    assert_eq!(code_of(&unreachable), "\"DAEMON_UNAVAILABLE\"");
}

/// A negative choice index is refused: the options are a zero-based list.
#[tokio::test]
async fn a_negative_choice_is_refused() {
    let answer = mutate(
        no_daemon_client(),
        r#"mutation { answerInteraction(request: { interactionId: "ask-1",
             answer: { choice: -1 } }) { outcome } }"#,
    )
    .await;
    assert!(
        answer.errors[0].message.contains("negative"),
        "{:?}",
        answer.errors
    );
}

/// Two answers at once is not a request the schema can express, which is what
/// makes the one-of worth having.
#[tokio::test]
async fn two_answers_at_once_are_not_a_request() {
    let answer = mutate(
        no_daemon_client(),
        r#"mutation { answerInteraction(request: { interactionId: "ask-1",
             answer: { text: "yes", choice: 1 } }) { outcome } }"#,
    )
    .await;
    assert!(!answer.errors.is_empty(), "two answers is not one answer");
}

// ── blueprints ──

/// A manifest exercising the blueprint writes.
fn manifest_text(name: &str, version: &str) -> String {
    format!(
        "[agent]\nname = \"{name}\"\nversion = \"{version}\"\ndescription = \"d\"\n\n\
         [stages.only]\nmode = \"autonomous\"\n"
    )
}

/// The manifest as a GraphQL string literal.
fn quoted_manifest(name: &str, version: &str) -> String {
    manifest_text(name, version)
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}

/// Installing a blueprint, then replacing it, then removing it.
///
/// The digest changes with the bytes, which is what tells a client the two
/// revisions apart, and what the pin on a later write is checked against.
#[tokio::test]
async fn a_blueprint_can_be_installed_replaced_and_removed() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let created = mutate(
            no_daemon_client(),
            &format!(
                "mutation {{ createBlueprint(request: {{ name: \"writer\", manifest: \"{}\" }}) \
                 {{ blueprint {{ name version digest source }} }} }}",
                quoted_manifest("writer", "1.0.0")
            ),
        )
        .await;
        assert!(created.errors.is_empty(), "{:?}", created.errors);
        let json = data_of(&created);
        let blueprint = &json["createBlueprint"]["blueprint"];
        assert_eq!(blueprint["name"], "writer");
        assert_eq!(blueprint["version"], "1.0.0");
        // A blueprint read from the installed set is never a run's snapshot.
        assert_eq!(blueprint["source"], "INSTALLED");
        let first_digest = blueprint["digest"].as_str().expect("a digest").to_string();

        // The pin holds, so the write goes through.
        let updated = mutate(
            no_daemon_client(),
            &format!(
                "mutation {{ updateBlueprint(request: {{ \
                 blueprint: {{ name: \"writer\", digest: \"{first_digest}\" }}, \
                 manifest: \"{}\" }}) {{ blueprint {{ version digest }} }} }}",
                quoted_manifest("writer", "2.0.0")
            ),
        )
        .await;
        assert!(updated.errors.is_empty(), "{:?}", updated.errors);
        let json = data_of(&updated);
        assert_eq!(json["updateBlueprint"]["blueprint"]["version"], "2.0.0");
        assert_ne!(
            json["updateBlueprint"]["blueprint"]["digest"]
                .as_str()
                .unwrap_or_default(),
            first_digest,
            "different bytes, different identity"
        );

        // The same pin again is stale now, and the write is refused with
        // nothing written.
        let stale = mutate(
            no_daemon_client(),
            &format!(
                "mutation {{ updateBlueprint(request: {{ \
                 blueprint: {{ name: \"writer\", digest: \"{first_digest}\" }}, \
                 manifest: \"{}\" }}) {{ blueprint {{ version }} }} }}",
                quoted_manifest("writer", "3.0.0")
            ),
        )
        .await;
        assert_eq!(code_of(&stale), "\"CONFLICT\"");

        let removed = mutate(
            no_daemon_client(),
            r#"mutation { deleteBlueprint(request: { blueprint: { name: "writer" } })
                 { deletedId } }"#,
        )
        .await;
        assert!(removed.errors.is_empty(), "{:?}", removed.errors);
        assert_eq!(data_of(&removed)["deleteBlueprint"]["deletedId"], "writer");
    })
    .await;
}

/// The refusals: a name already taken, a name that is not installed, a manifest
/// that will not parse, and a name that could escape the agents directory.
#[tokio::test]
async fn the_blueprint_writes_refuse_what_they_should() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let manifest = quoted_manifest("taken", "1.0.0");
        let create = |name: &str| {
            format!(
                "mutation {{ createBlueprint(request: {{ name: \"{name}\", \
                 manifest: \"{manifest}\" }}) {{ blueprint {{ name }} }} }}"
            )
        };
        let first = mutate(no_daemon_client(), &create("taken")).await;
        assert!(first.errors.is_empty(), "{:?}", first.errors);

        // Creating it again is a conflict: replacing somebody's agent is what
        // an edit is for.
        let again = mutate(no_daemon_client(), &create("taken")).await;
        assert_eq!(code_of(&again), "\"CONFLICT\"");

        // Editing one that is not installed is a miss, not a create.
        let missing = mutate(
            no_daemon_client(),
            &format!(
                "mutation {{ updateBlueprint(request: {{ blueprint: {{ name: \"ghost\" }}, \
                 manifest: \"{manifest}\" }}) {{ blueprint {{ name }} }} }}"
            ),
        )
        .await;
        assert_eq!(code_of(&missing), "\"NOT_FOUND\"");

        let unparseable = mutate(
            no_daemon_client(),
            r#"mutation { createBlueprint(request: { name: "broken",
                 manifest: "not a manifest" }) { blueprint { name } } }"#,
        )
        .await;
        assert!(
            unparseable.errors[0].message.contains("Invalid manifest"),
            "{:?}",
            unparseable.errors
        );

        let traversing = mutate(no_daemon_client(), &create("../escape")).await;
        assert!(
            traversing.errors[0]
                .message
                .contains("Invalid blueprint name"),
            "{:?}",
            traversing.errors
        );

        let gone = mutate(
            no_daemon_client(),
            r#"mutation { deleteBlueprint(request: { blueprint: { name: "ghost" } })
                 { deletedId } }"#,
        )
        .await;
        assert_eq!(code_of(&gone), "\"NOT_FOUND\"");
    })
    .await;
}

// ── exports ──

/// An export reads each run's record once: the walk that chose the runs is
/// carrying every one of them by the time the file is written.
///
/// Answering an export from the ids alone means opening the whole store a
/// second time, on the request's own task, for records the server already had
/// in hand.
#[tokio::test]
async fn an_export_reads_each_record_once() {
    crate::runstate::with_isolated_runs_dir_async("graphql-export-reads", |_d| async move {
        for at in 0..5 {
            create_run(&run_in(&format!("exp9-{at}"), RunStatus::Complete)).expect("run written");
        }
        let counted = || crate::commands::serve::testutil::records_read_under("exp9-");
        let agents = empty_agents();
        let state = state_with_agent_paths(vec![agents.path().to_path_buf()]);
        let schema = Schema::build(Query, Mutation::default(), EmptySubscription)
            .data(state)
            .finish();

        let before = counted();
        let started = schema
            .execute(Request::new(
                r#"mutation { startRunExport(request: { filter: { status: { eq: COMPLETE } } })
                     { export { id } } }"#,
            ))
            .await;
        assert!(started.errors.is_empty(), "{:?}", started.errors);
        assert_eq!(
            counted() - before,
            0,
            "no record the walk already held was opened again"
        );

        let id = data_of(&started)["startRunExport"]["export"]["id"]
            .as_str()
            .expect("an id")
            .to_string();
        // The file is still the whole selection: reading less must not mean
        // writing less.
        let mut written = None;
        for _ in 0..400 {
            let polled = schema
                .execute(Request::new(format!(
                    "{{ runExport(id: \"{id}\") {{ status written error }} }}"
                )))
                .await;
            assert!(polled.errors.is_empty(), "{:?}", polled.errors);
            let json = data_of(&polled);
            if json["runExport"]["status"] == "COMPLETE" {
                assert!(json["runExport"]["error"].is_null());
                written = json["runExport"]["written"].as_i64();
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(
            written,
            Some(5),
            "every run the filter named is in the file"
        );
    })
    .await;
}

/// An export is started, polled, and handed over as a signed link.
///
/// One schema for both halves on purpose: the job lives in this server's own
/// registry, so starting it and polling it have to be the same server.
#[tokio::test]
async fn an_export_is_started_then_polled_for_its_link() {
    crate::runstate::with_isolated_runs_dir_async("graphql-export", |_d| async move {
        create_run(&run_in("run-a", RunStatus::Complete)).expect("run written");
        create_run(&run_in("run-b", RunStatus::Running)).expect("run written");
        let agents = empty_agents();
        let state = state_with_agent_paths(vec![agents.path().to_path_buf()]);
        let schema = Schema::build(Query, Mutation::default(), EmptySubscription)
            .data(state)
            .finish();

        let started = schema
            .execute(Request::new(
                r#"mutation { startRunExport(request: { filter: { status: { in: [COMPLETE] } },
                     fields: ["run_id", "status"] })
                     { export { id status written downloadUrl } } }"#,
            ))
            .await;
        assert!(started.errors.is_empty(), "{:?}", started.errors);
        let json = data_of(&started);
        let export = &json["startRunExport"]["export"];
        let id = export["id"].as_str().expect("an id").to_string();
        assert!(
            export["downloadUrl"].is_null(),
            "nothing to fetch before it is written"
        );

        // Poll until the worker has finished: the mutation answers before the
        // file exists, which is the point of a job.
        let mut link = None;
        for _ in 0..200 {
            let polled = schema
                .execute(Request::new(format!(
                    "{{ runExport(id: \"{id}\") {{ status written error downloadUrl }} }}"
                )))
                .await;
            assert!(polled.errors.is_empty(), "{:?}", polled.errors);
            let json = data_of(&polled);
            if json["runExport"]["status"] == "COMPLETE" {
                assert_eq!(json["runExport"]["written"], 1, "the filter was applied");
                assert!(json["runExport"]["error"].is_null());
                link = json["runExport"]["downloadUrl"]
                    .as_str()
                    .map(str::to_string);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let link = link.expect("the export finished with a link");
        assert!(link.starts_with(&format!("/api/exports/{id}?")), "{link}");
    })
    .await;
}

/// An export with no filter is every run, and the page cap that governs a
/// response does not govern a file.
#[tokio::test]
async fn an_export_of_everything_is_not_a_page() {
    crate::runstate::with_isolated_runs_dir_async("graphql-export-all", |_d| async move {
        for i in 0..3 {
            create_run(&run_in(&format!("run-{i}"), RunStatus::Complete)).expect("run written");
        }
        let agents = empty_agents();
        let state = state_with_agent_paths(vec![agents.path().to_path_buf()]);
        let schema = Schema::build(Query, Mutation::default(), EmptySubscription)
            .data(state.clone())
            .finish();
        let started = schema
            .execute(Request::new(
                "mutation { startRunExport(request: {}) { export { id status } } }",
            ))
            .await;
        assert!(started.errors.is_empty(), "{:?}", started.errors);
        let json = data_of(&started);
        let id = json["startRunExport"]["export"]["id"]
            .as_str()
            .expect("an id");
        for _ in 0..200 {
            let job = state.caches.exports.get(id).expect("the job");
            if job.status == crate::commands::serve::core::export::ExportStatus::Complete {
                assert_eq!(job.written, 3, "every run, not one page of them");
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("the export did not finish");
    })
    .await;
}

/// A field that no run carries is refused, and nothing is written.
#[tokio::test]
async fn an_export_of_an_unknown_field_is_refused() {
    crate::runstate::with_isolated_runs_dir_async("graphql-export-field", |_d| async move {
        let answer = mutate(
            no_daemon_client(),
            r#"mutation { startRunExport(request: { fields: ["run_id", "nope"] })
                 { export { id } } }"#,
        )
        .await;
        assert!(
            answer.errors[0].message.contains("nope"),
            "{}",
            answer.errors[0].message
        );
        assert_eq!(code_of(&answer), "\"BAD_USER_INPUT\"");
    })
    .await;
}

// ── the catalogue ──

/// Refreshing the catalogue is a write, and it answers with the catalogue.
///
/// Nothing is configured here, so nothing is dialled and the answer is the
/// empty catalogue: what is asserted is that the field runs and reads back
/// through the same core call the listing uses.
#[tokio::test]
async fn refreshing_the_models_answers_with_the_catalogue() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let answer = mutate(
            no_daemon_client(),
            r#"mutation { refreshModels(request: { provider: "openai" })
                 { models { id providerId providerName } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = data_of(&answer);
        assert!(
            json["refreshModels"]["models"].is_array(),
            "a catalogue, even an empty one: {json}"
        );
    })
    .await;
}

// ── input shapes ──

/// Every input object round-trips through its own value form.
///
/// An input type is written for one direction and generated for both: the
/// schema reads one off the wire, and the executor writes one back when it
/// reports a bad value or fills a variable's default. A type whose two halves
/// disagree would report a rejected value as something the caller did not send.
#[test]
fn every_input_object_round_trips() {
    use async_graphql::InputType;

    let spawn = SpawnRunRequest {
        blueprint: BlueprintRef {
            name: "coder".to_string(),
            digest: None,
        },
        task: "fix the parser".to_string(),
        model: Some("gpt-5.6".to_string()),
        max_depth: Some(3),
        workdir: Some("/work".to_string()),
        yolo: Some(YoloWrite::ProfileName("cautious".to_string())),
        allow_tools: Some(vec!["shell".to_string()]),
        skip_seed_commands: true,
        capture_model_input: true,
        regions: Some(vec![RegionSeedWrite {
            region: RegionRef {
                name: "plan".to_string(),
            },
            text: "start here".to_string(),
        }]),
        metadata: Some(vec![KeyValueWrite {
            key: "ticket".to_string(),
            value: "42".to_string(),
        }]),
        output: Some(OutputRequestWrite {
            format: Some("json".to_string()),
            instructions: Some("one object".to_string()),
        }),
        callback: Some(CallbackWrite {
            url: "https://example.test/hook".to_string(),
            secret: Some("shh".to_string()),
        }),
        attachments: Some(vec![AttachmentWrite {
            path: "hero.png".to_string(),
            region: Some(RegionRef {
                name: "art".to_string(),
            }),
            name: Some("the-hero".to_string()),
            mime_type: Some("image/png".to_string()),
            deliver: Some(Delivery::StandIn),
            caption: Some("v1".to_string()),
        }]),
    };
    let Ok(read_back) = SpawnRunRequest::parse(Some(spawn.to_value())) else {
        panic!("a spawn request reads back from its own value");
    };
    assert_eq!(read_back.blueprint.name, "coder");
    assert_eq!(read_back.max_depth, Some(3));
    assert_eq!(
        read_back.regions.as_ref().map(Vec::len),
        Some(1),
        "the nested inputs come with it"
    );
    assert_eq!(read_back.metadata.as_ref().map(Vec::len), Some(1));
    assert_eq!(read_back.attachments.as_ref().map(Vec::len), Some(1));

    let answers = [
        InteractionAnswerWrite::Choice(1),
        InteractionAnswerWrite::Text("the words".to_string()),
        InteractionAnswerWrite::Approve(ApproveWrite {
            scope: ApprovalScope::Stage,
        }),
        InteractionAnswerWrite::Deny(DenyWrite {
            feedback: Some("try the other file".to_string()),
        }),
    ];
    for answer in answers {
        let request = AnswerInteractionRequest {
            interaction_id: async_graphql::ID::from("r1"),
            answer,
        };
        let value = request.to_value();
        let Ok(read_back) = AnswerInteractionRequest::parse(Some(value)) else {
            panic!("an answer reads back from its own value");
        };
        assert_eq!(read_back.interaction_id.as_str(), "r1");
    }

    // The waiver's other half, which the spawn above does not carry.
    let waived = YoloWrite::Everything(true);
    let Ok(read_back) = YoloWrite::parse(Some(waived.to_value())) else {
        panic!("a waiver reads back from its own value");
    };
    let YoloWrite::Everything(everything) = read_back else {
        panic!("the blanket waiver came back as the named one");
    };
    assert!(everything);
}

/// Every input object refuses what it cannot read.
///
/// All of them together, wherever their field lives: they are read by the same
/// derived code, and the question - does a wrong value fail the request rather
/// than land as a default - has one answer for all of them. Each type is asked
/// three ways, because each field is read in turn: not an object at all, no
/// value at all, and a first field that reads with a later one that does not.
#[test]
fn every_input_object_refuses_what_it_cannot_read() {
    use async_graphql::{InputType, Name, Value, indexmap::IndexMap};

    /// One object with a single field set to `value`.
    fn one(field: &str, value: Value) -> Option<Value> {
        let mut map = IndexMap::new();
        map.insert(Name::new(field), value);
        Some(Value::Object(map))
    }
    let scalar = || Some(Value::String("nope".to_string()));
    let number = || Value::Number(7.into());
    let text = |s: &str| Value::String(s.to_string());
    /// A one-field object, as the wire carries a reference.
    fn named(name: &str) -> Value {
        let mut map = IndexMap::new();
        map.insert(Name::new("name"), Value::String(name.to_string()));
        Value::Object(map)
    }

    assert!(KeyValueWrite::parse(scalar()).is_err());
    assert!(KeyValueWrite::parse(None).is_err());
    assert!(KeyValueWrite::parse(one("key", number())).is_err());
    assert!(
        KeyValueWrite::parse(one("key", text("k"))).is_err(),
        "no value"
    );

    assert!(RegionSeedWrite::parse(scalar()).is_err());
    assert!(RegionSeedWrite::parse(None).is_err());
    assert!(RegionSeedWrite::parse(one("region", number())).is_err());
    assert!(
        RegionSeedWrite::parse(one("region", named("plan"))).is_err(),
        "no text"
    );

    assert!(AttachmentWrite::parse(scalar()).is_err());
    assert!(AttachmentWrite::parse(None).is_err());
    assert!(AttachmentWrite::parse(one("path", number())).is_err());
    let mut attachment = IndexMap::new();
    attachment.insert(Name::new("path"), text("hero.png"));
    attachment.insert(Name::new("caption"), number());
    assert!(AttachmentWrite::parse(Some(Value::Object(attachment))).is_err());

    assert!(OutputRequestWrite::parse(scalar()).is_err());
    assert!(OutputRequestWrite::parse(None).is_err());
    assert!(OutputRequestWrite::parse(one("format", number())).is_err());

    assert!(CallbackWrite::parse(scalar()).is_err());
    assert!(CallbackWrite::parse(None).is_err());
    assert!(CallbackWrite::parse(one("url", number())).is_err());
    assert!(
        CallbackWrite::parse(one("secret", text("shh"))).is_err(),
        "a secret with nothing to sign for is not a callback"
    );
    let mut callback = IndexMap::new();
    callback.insert(Name::new("url"), text("https://example.test"));
    callback.insert(Name::new("secret"), number());
    assert!(CallbackWrite::parse(Some(Value::Object(callback))).is_err());

    assert!(YoloWrite::parse(scalar()).is_err());
    assert!(YoloWrite::parse(None).is_err());
    assert!(YoloWrite::parse(one("everything", text("yes"))).is_err());

    assert!(SpawnRunRequest::parse(scalar()).is_err());
    assert!(SpawnRunRequest::parse(None).is_err());
    assert!(SpawnRunRequest::parse(one("blueprint", number())).is_err());
    assert!(
        SpawnRunRequest::parse(one("blueprint", named("coder"))).is_err(),
        "no task"
    );
    assert!(
        SpawnRunRequest::parse(one("task", text("t"))).is_err(),
        "no blueprint"
    );
    // The last field of the largest input, which is read after every other one.
    let mut spawn = IndexMap::new();
    spawn.insert(Name::new("blueprint"), named("coder"));
    spawn.insert(Name::new("task"), text("fix it"));
    spawn.insert(Name::new("attachments"), number());
    assert!(SpawnRunRequest::parse(Some(Value::Object(spawn))).is_err());

    assert!(SendMessageRequest::parse(scalar()).is_err());
    assert!(SendMessageRequest::parse(None).is_err());
    assert!(SendMessageRequest::parse(one("runId", Value::List(Vec::new()))).is_err());
    assert!(
        SendMessageRequest::parse(one("runId", text("run-a"))).is_err(),
        "no words"
    );

    assert!(ApproveWrite::parse(scalar()).is_err());
    assert!(ApproveWrite::parse(None).is_err());
    assert!(ApproveWrite::parse(one("scope", number())).is_err());
    assert!(DenyWrite::parse(scalar()).is_err());
    assert!(DenyWrite::parse(None).is_err());
    assert!(DenyWrite::parse(one("feedback", number())).is_err());

    assert!(InteractionAnswerWrite::parse(scalar()).is_err());
    assert!(InteractionAnswerWrite::parse(None).is_err());
    assert!(
        InteractionAnswerWrite::parse(one("text", number())).is_err(),
        "the chosen answer still has to be an answer"
    );
    assert!(
        InteractionAnswerWrite::parse(one("approve", Value::Object(IndexMap::new()))).is_ok(),
        "an approval that says nothing is the default scope"
    );

    assert!(AnswerInteractionRequest::parse(scalar()).is_err());
    assert!(AnswerInteractionRequest::parse(None).is_err());
    assert!(
        AnswerInteractionRequest::parse(one("interactionId", Value::List(Vec::new()))).is_err()
    );
    assert!(
        AnswerInteractionRequest::parse(one("interactionId", text("r1"))).is_err(),
        "no answer"
    );

    assert!(DeleteRunsRequest::parse(scalar()).is_err());
    assert!(DeleteRunsRequest::parse(None).is_err());
    assert!(DeleteRunsRequest::parse(one("force", number())).is_err());

    assert!(PauseRunRequest::parse(scalar()).is_err());
    assert!(PauseRunRequest::parse(None).is_err());
    assert!(PauseRunRequest::parse(one("id", Value::List(Vec::new()))).is_err());
    assert!(PauseRunsRequest::parse(scalar()).is_err());
    assert!(PauseRunsRequest::parse(None).is_err());
    assert!(PauseRunsRequest::parse(one("filter", number())).is_err());

    assert!(CreateBlueprintRequest::parse(scalar()).is_err());
    assert!(CreateBlueprintRequest::parse(None).is_err());
    assert!(CreateBlueprintRequest::parse(one("name", number())).is_err());
    assert!(
        CreateBlueprintRequest::parse(one("name", text("writer"))).is_err(),
        "no manifest"
    );
    assert!(UpdateBlueprintRequest::parse(scalar()).is_err());
    assert!(UpdateBlueprintRequest::parse(None).is_err());
    assert!(UpdateBlueprintRequest::parse(one("blueprint", number())).is_err());
    assert!(
        UpdateBlueprintRequest::parse(one("blueprint", named("writer"))).is_err(),
        "no manifest"
    );
    assert!(DeleteBlueprintRequest::parse(scalar()).is_err());
    assert!(DeleteBlueprintRequest::parse(None).is_err());
    assert!(DeleteBlueprintRequest::parse(one("blueprint", number())).is_err());

    assert!(StartRunExportRequest::parse(scalar()).is_err());
    assert!(StartRunExportRequest::parse(None).is_err());
    assert!(StartRunExportRequest::parse(one("fields", number())).is_err());

    assert!(RefreshModelsRequest::parse(scalar()).is_err());
    assert!(RefreshModelsRequest::parse(None).is_err());
    assert!(RefreshModelsRequest::parse(one("provider", number())).is_err());

    // A request carried as a field of another input, which is the path a
    // client takes when it builds one in code rather than inline.
    #[derive(async_graphql::InputObject)]
    struct SpawnProbe {
        /// The spawn being carried.
        request: SpawnRunRequest,
    }
    let mut carried = IndexMap::new();
    carried.insert(
        Name::new("request"),
        SpawnRunRequest {
            blueprint: BlueprintRef {
                name: "coder".to_string(),
                digest: None,
            },
            task: "fix the parser".to_string(),
            model: None,
            max_depth: None,
            workdir: None,
            yolo: None,
            allow_tools: None,
            skip_seed_commands: false,
            capture_model_input: false,
            regions: None,
            metadata: None,
            output: None,
            callback: None,
            attachments: None,
        }
        .to_value(),
    );
    let Ok(probe) = SpawnProbe::parse(Some(Value::Object(carried))) else {
        panic!("a spawn request reads back as a carried field");
    };
    assert_eq!(probe.request.task, "fix the parser");
    let mut broken = IndexMap::new();
    broken.insert(Name::new("request"), text("coder"));
    assert!(SpawnProbe::parse(Some(Value::Object(broken))).is_err());
}

// ── waiting for an act to show in the record ──

/// Each act has its own idea of what landing looks like. A resume is the odd
/// one: what the run goes back to doing is its own business, so the only thing
/// the resume promises is that it is no longer parked.
#[test]
fn each_act_knows_what_landing_looks_like() {
    use crate::commands::serve::core::lifecycle::Action;
    assert!(has_landed(Action::Pause, &RunStatus::Paused));
    assert!(!has_landed(Action::Pause, &RunStatus::Running));
    assert!(has_landed(Action::Cancel, &RunStatus::Cancelled));
    assert!(!has_landed(Action::Cancel, &RunStatus::Running));
    assert!(has_landed(Action::Resume, &RunStatus::Running));
    assert!(has_landed(Action::Resume, &RunStatus::WaitingInput));
    assert!(!has_landed(Action::Resume, &RunStatus::Paused));
}

/// A record that already shows the act is answered on the first look, with no
/// waiting at all - which is what stops a pause on an already-paused run from
/// sitting out the whole window.
#[tokio::test]
async fn a_record_that_already_shows_the_act_is_answered_at_once() {
    use crate::commands::serve::core::lifecycle::Action;
    let looks = std::cell::Cell::new(0);
    let settled = settle(
        Action::Pause,
        std::time::Instant::now() + std::time::Duration::from_secs(30),
        || {
            looks.set(looks.get() + 1);
            Ok(run_in("run-a", RunStatus::Paused))
        },
    )
    .await
    .expect("the record reads");
    assert_eq!(settled.status, RunStatus::Paused);
    assert_eq!(looks.get(), 1, "one look, no waiting");
}

/// The record catches up a moment later, which is the case the window exists
/// for: the first look still shows the status the run held when it was asked.
#[tokio::test]
async fn a_record_that_catches_up_is_waited_for() {
    use crate::commands::serve::core::lifecycle::Action;
    let looks = std::cell::Cell::new(0);
    let settled = settle(
        Action::Cancel,
        std::time::Instant::now() + std::time::Duration::from_secs(30),
        || {
            looks.set(looks.get() + 1);
            Ok(match looks.get() {
                1 => run_in("run-a", RunStatus::Running),
                _ => run_in("run-a", RunStatus::Cancelled),
            })
        },
    )
    .await
    .expect("the record reads");
    assert_eq!(settled.status, RunStatus::Cancelled);
    assert_eq!(looks.get(), 2, "looked again once the first was stale");
}

/// A window that closes before the act shows answers with the record as it
/// stands rather than failing: the act was accepted, and the caller is told
/// what is there.
#[tokio::test]
async fn a_window_that_closes_answers_with_what_is_there() {
    use crate::commands::serve::core::lifecycle::Action;
    let settled = settle(
        Action::Pause,
        std::time::Instant::now() - std::time::Duration::from_millis(1),
        || Ok(run_in("run-a", RunStatus::Running)),
    )
    .await
    .expect("the record reads");
    assert_eq!(settled.status, RunStatus::Running);
}

/// A record that will not read is this server's problem, and it says so instead
/// of answering with a run it did not read.
#[tokio::test]
async fn a_record_that_will_not_read_is_reported() {
    use crate::commands::serve::core::error::ServeError;
    use crate::commands::serve::core::lifecycle::Action;
    let failure = settle(
        Action::Pause,
        std::time::Instant::now() + std::time::Duration::from_secs(30),
        || {
            Err(ServeError::Internal(
                "the record would not read".to_string(),
            ))
        },
    )
    .await
    .expect_err("the read failed");
    assert_eq!(failure.code(), "INTERNAL");
}

/// Every write shape a mutation takes reads back from its own value, and
/// refuses a field of the wrong type.
///
/// An input type is written for one direction and generated for both, and only
/// a field that will not read walks the half a valid request never does.
#[test]
fn every_write_shape_round_trips() {
    use super::super::filter::testkit::round_trip;
    use super::super::types::interaction::ApprovalScope;
    use super::super::types::run::RunFilter;
    use super::interactions::{ApproveWrite, DenyWrite, InteractionAnswerWrite};
    use super::runs::{DeleteRunsRequest, OutputRequestWrite, SendMessageRequest, YoloWrite};

    round_trip(&YoloWrite::Everything(true));
    round_trip(&YoloWrite::ProfileName("bare".to_string()));
    round_trip(&OutputRequestWrite {
        format: Some("markdown".to_string()),
        instructions: Some("short".to_string()),
    });
    round_trip(&SendMessageRequest {
        run_id: async_graphql::ID::from("x0000000000000000-00000000"),
        text: "carry on".to_string(),
        region: None,
        attachments: None,
    });
    round_trip(&DeleteRunsRequest {
        filter: RunFilter::default(),
        force: true,
    });
    for answer in [
        InteractionAnswerWrite::Choice(0),
        InteractionAnswerWrite::Text("yes".to_string()),
        InteractionAnswerWrite::Approve(ApproveWrite {
            scope: ApprovalScope::Once,
        }),
        InteractionAnswerWrite::Deny(DenyWrite {
            feedback: Some("no".to_string()),
        }),
    ] {
        round_trip(&answer);
    }
}

/// Every way an attachment can be asked to reach the model maps across to the
/// run's own vocabulary.
///
/// Its own test rather than one per spawn: reaching each of these through a
/// spawn would be three runs to prove one mapping.
#[test]
fn every_delivery_maps_across() {
    use super::attachments::Delivery;
    use leviath_core::mime::Delivery as Wanted;

    assert_eq!(Wanted::from(Delivery::Native), Wanted::Native);
    assert_eq!(Wanted::from(Delivery::Text), Wanted::Text);
    assert_eq!(Wanted::from(Delivery::StandIn), Wanted::StandIn);
}

/// The three ways a message carrying files, or landing on a run whose record
/// has gone, is refused.
///
/// A message is the one write that reads the run's record twice: once to find
/// the working directory its attachments are named inside, and once to answer
/// with the run as it now stands.
#[tokio::test]
async fn a_message_is_refused_by_its_files_and_by_a_record_that_is_gone() {
    crate::runstate::with_isolated_runs_dir_async("graphql-message-files", |_d| async move {
        let workdir = tempfile::tempdir().expect("a workdir");
        std::fs::write(workdir.path().join("notes.txt"), "hello").expect("a file");
        let mut meta = run_in("run-a", RunStatus::WaitingInput);
        meta.workdir = workdir.path().to_string_lossy().to_string();
        create_run(&meta).expect("run written");

        // A path outside the run's own working directory never reaches the
        // daemon.
        let escaping = mutate(
            no_daemon_client(),
            r#"mutation { sendMessage(request: { runId: "run-a", text: "look",
                 attachments: [{ path: "../escape.png" }] }) { run { id } } }"#,
        )
        .await;
        assert!(!escaping.errors.is_empty(), "a path outside the workdir");

        // A declared type that is not a mime type at all is refused where the
        // file is read, naming the part it came from.
        let mistyped = mutate(
            no_daemon_client(),
            r#"mutation { sendMessage(request: { runId: "run-a", text: "look",
                 attachments: [{ path: "notes.txt", mimeType: "nonsense" }] })
                 { run { id } } }"#,
        )
        .await;
        assert!(
            mistyped.errors[0].message.contains("which is not one"),
            "{}",
            mistyped.errors[0].message
        );

        // The daemon takes a message for a run this server has no record of,
        // and the answer is the read that then fails rather than a null run.
        let (control, _dir, _srv) = fake_daemon(|_| ControlResponse::Ok { ok: true });
        let vanished = mutate(
            control,
            r#"mutation { sendMessage(request: { runId: "ghost", text: "hi" })
                 { run { id } } }"#,
        )
        .await;
        assert!(
            !vanished.errors.is_empty(),
            "a run with no record cannot be answered with"
        );
    })
    .await;
}

/// A blueprint reference that pins a revision nothing has is refused by the
/// delete, before anything is removed.
#[tokio::test]
async fn a_delete_of_a_stale_pin_is_refused() {
    crate::commands::serve::testutil::with_home(|_home| async move {
        let manifest = quoted_manifest("pinned", "1.0.0");
        let created = mutate(
            no_daemon_client(),
            &format!(
                "mutation {{ createBlueprint(request: {{ name: \"pinned\", \
                 manifest: \"{manifest}\" }}) {{ blueprint {{ name }} }} }}"
            ),
        )
        .await;
        assert!(created.errors.is_empty(), "{:?}", created.errors);

        let stale = mutate(
            no_daemon_client(),
            r#"mutation { deleteBlueprint(request: {
                 blueprint: { name: "pinned", digest: "0000000000000000" } })
                 { deletedId } }"#,
        )
        .await;
        assert_eq!(code_of(&stale), "\"CONFLICT\"");
    })
    .await;
}

/// A sweep that names more runs than one delete may carry is refused by the
/// delete itself, after the filter has already agreed to them.
///
/// The filter's own cap is on the ids a request writes out; this one is on what
/// the walk found, which is the other half of the same rule.
#[tokio::test]
async fn a_sweep_wider_than_one_delete_may_carry_is_refused() {
    crate::runstate::with_isolated_runs_dir_async("graphql-delete-cap", |_d| async move {
        for at in 0..=crate::commands::serve::core::runs::MAX_IDS {
            create_run(&run_in(&format!("run-{at:04}"), RunStatus::Complete)).expect("run written");
        }
        let answer = mutate(
            no_daemon_client(),
            "mutation { deleteRuns(request: { filter: { status: { eq: COMPLETE } } }) \
             { deletedIds } }",
        )
        .await;
        assert_eq!(code_of(&answer), "\"BAD_USER_INPUT\"");
        assert!(
            answer.errors[0].message.contains("at most"),
            "it says what the cap is: {}",
            answer.errors[0].message
        );
    })
    .await;
}

/// An export takes the listing's filter, so it refuses the filters the listing
/// refuses before it queues anything.
#[tokio::test]
async fn an_export_refuses_a_filter_too_deep_to_walk() {
    crate::runstate::with_isolated_runs_dir_async("graphql-export-deep", |_d| async move {
        create_run(&run_in("run-a", RunStatus::Complete)).expect("run written");
        let mut deep = "{ task: { eq: \"x\" } }".to_string();
        for _ in 0..crate::commands::serve::graphql::paging::digest::MAX_FILTER_DEPTH {
            deep = format!("{{ and: [{deep}] }}");
        }
        let refused = mutate(
            no_daemon_client(),
            &format!(
                "mutation {{ startRunExport(request: {{ filter: {deep} }}) \
                 {{ export {{ id }} }} }}"
            ),
        )
        .await;
        assert_eq!(code_of(&refused), "\"BAD_USER_INPUT\"");
        assert!(
            refused.errors[0].message.contains("levels deep"),
            "{}",
            refused.errors[0].message
        );
    })
    .await;
}
