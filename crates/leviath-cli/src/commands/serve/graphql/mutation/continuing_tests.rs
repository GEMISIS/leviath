//! `continueRun`, and `spawnRun` and `validateSpawn` with
//! `source.continueFrom`, run whole against a fake daemon.

use async_graphql::{EmptySubscription, Request, Schema};
use leviath_runtime::control_socket::{ControlClient, ControlRequest, ControlResponse};

use super::super::Mutation;
use crate::commands::serve::graphql::query::Query;
use crate::commands::serve::testutil::{fake_daemon, no_daemon_client, state_with_agent_paths};
use crate::runstate::{RunMeta, RunStatus, create_run};

async fn mutate(control: ControlClient, query: &str) -> async_graphql::Response {
    let agents = tempfile::tempdir().expect("a temp agents dir");
    let mut state = state_with_agent_paths(vec![agents.path().to_path_buf()]);
    state.control = control;
    let schema = Schema::build(Query, Mutation::default(), EmptySubscription)
        .data(state)
        .finish();
    schema.execute(Request::new(query)).await
}

fn code_of(answer: &async_graphql::Response) -> String {
    answer.errors[0]
        .extensions
        .as_ref()
        .and_then(|extensions| extensions.get("code"))
        .map(ToString::to_string)
        .unwrap_or_default()
}

fn data_of(answer: &async_graphql::Response) -> serde_json::Value {
    serde_json::to_value(&answer.data).expect("data serializes")
}

/// A finished run on disk, working in `workdir`.
fn finished(id: &str, workdir: &std::path::Path) {
    let mut meta = RunMeta::new(
        id.to_string(),
        "test-agent".to_string(),
        "/agents/test".to_string(),
        "do the thing".to_string(),
        None,
        workdir.to_string_lossy().to_string(),
        1,
    );
    meta.status = RunStatus::Complete;
    create_run(&meta).expect("run written");
}

/// A daemon that continues whatever it is asked to, into a new run for a
/// fork.
fn continuing(request: ControlRequest) -> ControlResponse {
    match request {
        ControlRequest::Continue {
            run_id,
            fork,
            parts,
            ..
        } => {
            assert!(parts.iter().all(|p| p.name == "notes.txt"));
            ControlResponse::Continued {
                run_id: match fork {
                    true => "run-b".into(),
                    false => run_id.clone(),
                },
                segment: if fork { 1 } else { 2 },
                forked_from: fork.then_some(run_id),
            }
        }
        other => panic!("a continuation reaches the daemon: {other:?}"),
    }
}

fn conflict(_: ControlRequest) -> ControlResponse {
    ControlResponse::Rejected {
        issues: leviath_runtime::continuation::refused(
            leviath_runtime::spec::issues::IssueCode::Conflict,
            leviath_runtime::spec::issues::SpecPath::root(),
            "run 'run-a' has not finished",
        ),
    }
}

#[tokio::test]
async fn continuing_a_run_answers_with_the_run_and_its_segment() {
    crate::runstate::with_isolated_runs_dir_async("graphql-continue", |_d| async move {
        let workdir = tempfile::tempdir().unwrap();
        std::fs::write(workdir.path().join("notes.txt"), b"read this").unwrap();
        finished("run-a", workdir.path());
        let (control, _dir, _srv) = fake_daemon(continuing);
        let answer = mutate(
            control,
            r#"mutation { continueRun(request: { runId: "run-a", text: "make it red",
                 region: { name: "plan" }, attachments: [{ path: "notes.txt" }] })
                 { run { id } segment seq } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = data_of(&answer);
        assert_eq!(json["continueRun"]["run"]["id"], "run-a");
        assert_eq!(json["continueRun"]["segment"], 2);
    })
    .await;
}

#[tokio::test]
async fn a_refused_or_unanswerable_continuation_is_an_error_of_its_kind() {
    crate::runstate::with_isolated_runs_dir_async("graphql-continue-refused", |_d| async move {
        let query = r#"mutation { continueRun(request: { runId: "run-a", text: "m" })
                 { segment } }"#;
        let (control, _dir, _srv) = fake_daemon(conflict);
        assert_eq!(code_of(&mutate(control, query).await), "\"CONFLICT\"");
        assert!(!mutate(no_daemon_client(), query).await.errors.is_empty());
        // Continued, but with no record to read back.
        let (control, _dir, _srv) = fake_daemon(continuing);
        assert_eq!(code_of(&mutate(control, query).await), "\"INTERNAL\"");
        // Files named in a run this server has no record of.
        let answer = mutate(
            no_daemon_client(),
            r#"mutation { continueRun(request: { runId: "ghost", text: "m",
                 attachments: [{ path: "notes.txt" }] }) { segment } }"#,
        )
        .await;
        assert_eq!(code_of(&answer), "\"NOT_FOUND\"");
    })
    .await;
}

const SPAWN: &str = r#"
  ... on SpawnedOutput { runId deduplicated }
  ... on SpawnRejectedOutput { issues { path code message } }
"#;

#[tokio::test]
async fn a_fork_is_spawned_from_a_finished_run() {
    crate::runstate::with_isolated_runs_dir_async("graphql-fork", |_d| async move {
        let (control, _dir, _srv) = fake_daemon(continuing);
        let answer = mutate(
            control,
            &format!(
                r#"mutation {{ spawnRun(request: {{ source: {{ continueFrom: {{
                     runId: "run-a", text: "now facing left" }} }} }}) {{ {SPAWN} }} }}"#
            ),
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let json = data_of(&answer);
        assert_eq!(json["spawnRun"]["runId"], "run-b");
        assert_eq!(json["spawnRun"]["deduplicated"], false);

        let (control, _dir, _srv) = fake_daemon(conflict);
        let answer = mutate(
            control,
            &format!(
                r#"mutation {{ spawnRun(request: {{ source: {{ continueFrom: {{
                     runId: "run-a", text: "m" }} }} }}) {{ {SPAWN} }} }}"#
            ),
        )
        .await;
        assert_eq!(
            data_of(&answer)["spawnRun"]["issues"][0]["code"],
            "CONFLICT"
        );
    })
    .await;
}

/// A fork runs the finished run's own settings, so every other field of the
/// request is refused, each at its own path; and a fork is only checked by
/// starting it.
#[tokio::test]
async fn a_fork_takes_none_of_the_other_settings_and_no_dry_run() {
    crate::runstate::with_isolated_runs_dir_async("graphql-fork-refused", |_d| async move {
        let answer = mutate(
            no_daemon_client(),
            &format!(
                r#"mutation {{ spawnRun(request: {{
                     source: {{ continueFrom: {{ runId: "run-a", text: "m" }} }},
                     inputs: [{{ name: "task", value: {{ text: "x" }} }}],
                     attachments: [{{ name: "a", content: {{ base64: "AA==" }} }}],
                     model: "m", output: {{ format: "markdown" }}, workdir: "/w",
                     launch: {{ maxDepth: 1 }}, delivery: {{ metadata: [] }}
                   }}) {{ {SPAWN} }} }}"#
            ),
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let issues = &data_of(&answer)["spawnRun"]["issues"];
        let paths: Vec<&str> = issues
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["path"].as_str().unwrap())
            .collect();
        assert_eq!(
            paths,
            [
                "inputs",
                "attachments",
                "model",
                "output",
                "workdir",
                "launch",
                "delivery"
            ]
        );

        let answer = mutate(
            no_daemon_client(),
            r#"mutation { validateSpawn(request: { source: { continueFrom: {
                 runId: "run-a", text: "m" } } }) {
                 ... on SpawnRejectedOutput { issues { path code } } } }"#,
        )
        .await;
        assert!(answer.errors.is_empty(), "{:?}", answer.errors);
        let issue = &data_of(&answer)["validateSpawn"]["issues"][0];
        assert_eq!(issue["path"], "source.continue_from");
        assert_eq!(issue["code"], "NOT_ALLOWED");

        // A daemon that cannot be asked, and files in a run with no record.
        let fork = r#"mutation { spawnRun(request: { source: { continueFrom: {
                 runId: "ghost", text: "m" } } }) { __typename } }"#;
        assert!(!mutate(no_daemon_client(), fork).await.errors.is_empty());
        let with_files = r#"mutation { spawnRun(request: { source: { continueFrom: {
                 runId: "ghost", text: "m", attachments: [{ path: "a.txt" }] } } })
                 { __typename } }"#;
        assert_eq!(
            code_of(&mutate(no_daemon_client(), with_files).await),
            "\"NOT_FOUND\""
        );
    })
    .await;
}

/// Each continuation input reads back from its own value, with every field
/// set, and refuses a field of the wrong type.
#[test]
fn every_continuation_input_round_trips() {
    use super::super::spawn_request::SpawnSourceWrite;
    use super::{ContinueFromWrite, ContinueRunRequest};
    use crate::commands::serve::graphql::filter::testkit::round_trip;
    use crate::commands::serve::graphql::inputs::RegionRef;
    use crate::commands::serve::graphql::mutation::attachments::{AttachmentWrite, Delivery};

    let region = || {
        Some(RegionRef {
            name: "notes".to_string(),
        })
    };
    let attachments = || {
        Some(vec![AttachmentWrite {
            path: "hero.png".to_string(),
            region: region(),
            name: Some("hero".to_string()),
            mime_type: Some("image/png".to_string()),
            deliver: Some(Delivery::Native),
            caption: Some("the hero".to_string()),
        }])
    };
    round_trip(&ContinueRunRequest {
        run_id: async_graphql::ID::from("r-1"),
        text: "again".to_string(),
        region: region(),
        attachments: attachments(),
    });
    let from = || ContinueFromWrite {
        run_id: async_graphql::ID::from("r-1"),
        text: "again".to_string(),
        region: region(),
        attachments: attachments(),
    };
    round_trip(&from());
    round_trip(&SpawnSourceWrite::ContinueFrom(from()));
}
