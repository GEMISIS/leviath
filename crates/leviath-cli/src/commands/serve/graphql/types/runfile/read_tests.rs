//! Tests for reading a run's file through `Run.spec`, `Run.state`,
//! `Run.deltas` and `Run.graph`: a run recorded the way the daemon records
//! one, given a few steps of history, and read back.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_graphql::{EmptyMutation, EmptySubscription, Request, Schema};
use leviath_runtime::control_socket::{ControlClient, ControlRequest, ControlResponse};
use leviath_runtime::runfile::codec::{FrameKind, encode};
use leviath_runtime::runfile::{CheckpointPolicy, RunFileReader, RunFileWriter};
use leviath_runtime::state::{RunEvent, RunStatus, TransitionReason, TransitionRecord};

use super::super::read;
use crate::commands::serve::graphql::query::Query;
use crate::commands::serve::graphql::types::execution::ToolExecution;
use crate::commands::serve::testutil::{fake_daemon, no_daemon_client, state_with_agent_paths};
use crate::commands::serve::types::AppState;
use crate::config::Config;
use crate::daemon::starter::testing::{manifest_in, run_on_disk};

/// The run file of `run_id` under `runs`.
fn file_of(runs: &Path, run_id: &str) -> PathBuf {
    runs.join(run_id).join(leviath_core::files::RUN_FILE)
}

/// A run of the coder-shaped blueprint recorded under `runs`, then walked
/// analyze → implement by name, implement → review by the stages alone, and
/// forced back to analyze where no edge goes, finishing there. Returns its id.
fn walked(runs: &Path) -> String {
    let agent = tempfile::tempdir().expect("an agent dir").keep();
    let manifest = manifest_in(&agent, &crate::test_support::inline_coder_manifest());
    let mut registry = leviath_runtime::ProviderRegistry::new();
    registry.register(
        "anthropic".to_string(),
        Arc::new(crate::test_support::FakeProvider::new().context_window(100_000)),
    );
    let run_id = run_on_disk(Config::default(), registry, runs, &manifest);
    let path = file_of(runs, &run_id);
    let reader = RunFileReader::read(&path).expect("the run file reads");
    let edges = reader.spec().graph.edges.clone();
    let first = edges
        .iter()
        .find(|edge| edge.from.as_str() == "analyze")
        .expect("an edge out of analyze")
        .clone();
    let second = edges
        .iter()
        .find(|edge| edge.from.as_str() == "implement")
        .expect("an edge out of implement")
        .clone();
    let mut writer = RunFileWriter::open(
        &path,
        CheckpointPolicy {
            every: 2,
            size_ratio: 1_000.0,
        },
    )
    .expect("the run file opens for writing");
    let mut state = writer.state().clone();
    let moves = [
        (
            first.from.clone(),
            first.to.clone(),
            Some(first.name.clone()),
        ),
        (second.from.clone(), second.to.clone(), None),
        (second.to.clone(), first.from.clone(), None),
    ];
    for (at, (from, to, edge)) in moves.into_iter().enumerate() {
        state.cursor.stage = to.clone();
        *state.visits.entry(to.clone()).or_insert(0) += 1;
        state.last_transition = Some(TransitionRecord {
            from,
            to,
            edge,
            reason: TransitionReason::Condition,
            visit: format!("v{at}"),
        });
        if at == 2 {
            state.status = RunStatus::Complete;
        }
        writer
            .record(
                state.clone(),
                100 + at as i64,
                vec![
                    RunEvent::Log(format!("step {at}")),
                    RunEvent::Transition(state.last_transition.clone().expect("a move")),
                ],
            )
            .expect("the step is written");
    }
    run_id
}

/// A schema over a state that talks to `control`.
fn schema(control: ControlClient) -> Schema<Query, EmptyMutation, EmptySubscription> {
    let mut state = state_with_agent_paths(Vec::new());
    state.control = control;
    Schema::build(Query, EmptyMutation, EmptySubscription)
        .data(state)
        .finish()
}

/// One query's answer as JSON.
async fn ask(control: ControlClient, query: &str) -> serde_json::Value {
    let answer = schema(control).execute(Request::new(query)).await;
    serde_json::to_value(&answer).expect("the answer serializes")
}

/// The run's spec, its state now and at a step, its steps and its graph all
/// read off its file.
#[tokio::test]
async fn a_recorded_run_reads_back_whole() {
    crate::runstate::with_isolated_runs_dir_async("graphql-runfile-read", |_base| async move {
        let runs = crate::runstate::runs_dir();
        let run_id = walked(&runs);
        let answer = ask(
            no_daemon_client(),
            &format!(
                r#"{{ run(id: "{run_id}") {{
                    spec {{ runId origin {{ kind blueprintName }} stages {{ stage provider }} graph }}
                    now: state {{ seq status cursor {{ stage }} }}
                    start: state(at: 0) {{ seq cursor {{ stage }} }}
                    middle: state(at: 1) {{ seq cursor {{ stage }} lastTransition {{ edge }} }}
                    all: deltas {{ seq at changes {{ __typename }}
                                  events {{ ... on LogStepOutput {{ line }} }} }}
                    some: deltas(from: 2, to: 3) {{ seq }}
                    graph {{ nodes {{ stage visits current }}
                             edges {{ from to name condition reason taken }} }}
                }} }}"#
            ),
        )
        .await;
        assert_eq!(answer["errors"], serde_json::Value::Null, "{answer}");
        let run = &answer["data"]["run"];
        assert_eq!(run["spec"]["runId"], run_id.as_str());
        assert_eq!(run["spec"]["origin"]["kind"], "BLUEPRINT_FILE");
        assert_eq!(run["spec"]["origin"]["blueprintName"], "coder");
        assert_eq!(run["spec"]["stages"][0]["provider"], "anthropic");
        assert_eq!(run["now"]["seq"], 3);
        assert_eq!(run["now"]["status"], "COMPLETE");
        assert_eq!(run["now"]["cursor"]["stage"], "analyze");
        assert_eq!(run["start"]["seq"], 0);
        assert_eq!(run["middle"]["seq"], 1);
        assert_eq!(run["middle"]["cursor"]["stage"], "implement");
        assert!(run["middle"]["lastTransition"]["edge"].is_string(), "{run}");
        assert_eq!(run["all"].as_array().map(Vec::len), Some(3), "{run}");
        assert_eq!(run["all"][0]["events"][0]["line"], "step 0");
        assert_eq!(run["some"], serde_json::json!([{ "seq": 2 }, { "seq": 3 }]));
        let edges = run["graph"]["edges"].as_array().cloned().unwrap_or_default();
        let taken = |from: &str| {
            edges
                .iter()
                .filter(|edge| edge["from"] == from)
                .map(|edge| edge["taken"].as_i64().unwrap_or_default())
                .sum::<i64>()
        };
        assert_eq!(taken("analyze"), 1, "{edges:?}");
        assert_eq!(taken("implement"), 1, "{edges:?}");
        // The move out of `review` joins no declared edge, so it is shown as
        // one of its own: no name, no condition, the reason it was made.
        let forced: Vec<&serde_json::Value> = edges
            .iter()
            .filter(|edge| edge["from"] == "review")
            .collect();
        assert_eq!(forced.len(), 1, "{edges:?}");
        assert_eq!(forced[0]["taken"], 1);
        assert_eq!(forced[0]["name"], serde_json::Value::Null);
        assert_eq!(forced[0]["condition"], serde_json::Value::Null);
        assert_eq!(forced[0]["reason"], "CONDITION");
        let current: Vec<&serde_json::Value> = run["graph"]["nodes"]
            .as_array()
            .map(|nodes| nodes.iter().filter(|node| node["current"] == true).collect())
            .unwrap_or_default();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0]["stage"], "analyze");
    })
    .await;
}

/// A run the daemon holds answers `state` with the daemon's own view.
#[tokio::test]
async fn a_live_run_is_read_from_the_daemon() {
    crate::runstate::with_isolated_runs_dir_async("graphql-runfile-live", |_base| async move {
        let runs = crate::runstate::runs_dir();
        let run_id = walked(&runs);
        let mut live = RunFileReader::read(&file_of(&runs, &run_id))
            .expect("the run file reads")
            .latest_state()
            .expect("a state");
        live.seq = 42;
        let wanted = run_id.clone();
        let (control, _dir, _srv) = fake_daemon(move |request| {
            assert_eq!(
                request,
                ControlRequest::Inspect {
                    run_id: wanted.clone()
                }
            );
            ControlResponse::State {
                state: Box::new(live.clone()),
            }
        });
        let answer = ask(
            control,
            &format!(r#"{{ run(id: "{run_id}") {{ state {{ seq }} }} }}"#),
        )
        .await;
        assert_eq!(answer["data"]["run"]["state"]["seq"], 42, "{answer}");
    })
    .await;
}

/// The steps a client may not ask for are refused with the reason, and with
/// the code and status REST answers the same request with: a step past the
/// run's last is a window the file does not hold (416), and the rest are
/// wrong as written (400).
#[tokio::test]
async fn a_step_the_run_does_not_have_is_refused() {
    crate::runstate::with_isolated_runs_dir_async("graphql-runfile-steps", |_base| async move {
        let runs = crate::runstate::runs_dir();
        let run_id = walked(&runs);
        for (field, says, code, status) in [
            (
                "state(at: 9) { seq }",
                "has no step 9",
                "RANGE_NOT_SATISFIABLE",
                416,
            ),
            (
                "state(at: -1) { seq }",
                "`at` is never negative",
                "BAD_USER_INPUT",
                400,
            ),
            (
                "deltas(from: -1) { seq }",
                "`from` is never negative",
                "BAD_USER_INPUT",
                400,
            ),
            (
                "deltas(to: -1) { seq }",
                "`to` is never negative",
                "BAD_USER_INPUT",
                400,
            ),
            (
                "deltas(from: 3, to: 1) { seq }",
                "is before",
                "BAD_USER_INPUT",
                400,
            ),
            (
                "deltas(from: 1, to: 500) { seq }",
                "at most 200 steps",
                "BAD_USER_INPUT",
                400,
            ),
        ] {
            let answer = ask(
                no_daemon_client(),
                &format!(r#"{{ run(id: "{run_id}") {{ {field} }} }}"#),
            )
            .await;
            let error = &answer["errors"][0];
            assert_eq!(error["extensions"]["code"], code, "{field}: {answer}");
            assert_eq!(
                error["extensions"]["httpStatus"], status,
                "{field}: {answer}"
            );
            assert!(
                error["message"].as_str().is_some_and(|m| m.contains(says)),
                "{field}: {answer}"
            );
        }
    })
    .await;
}

/// A run with no run file has no spec, state or graph, and no steps.
#[tokio::test]
async fn a_run_without_a_run_file_has_nothing_to_read() {
    crate::runstate::with_isolated_runs_dir_async("graphql-runfile-none", |_runs| async move {
        let state: AppState = state_with_agent_paths(Vec::new());
        assert!(read::spec("older").await.expect("a miss").is_none());
        assert!(
            read::state(&state, "older", Some(0))
                .await
                .expect("a miss")
                .is_none()
        );
        assert!(
            read::deltas("older", None, None)
                .await
                .expect("a miss")
                .is_empty()
        );
        assert!(read::graph("older").await.expect("a miss").is_none());
    })
    .await;
}

/// A run file this server cannot read, and one it cannot open, are failures
/// with their own codes.
#[tokio::test]
async fn a_run_file_that_will_not_read_is_a_failure() {
    crate::runstate::with_isolated_runs_dir_async("graphql-runfile-bad", |_base| async move {
        let runs = crate::runstate::runs_dir();
        let garbled = runs.join("garbled");
        std::fs::create_dir_all(&garbled).expect("a run dir");
        std::fs::write(
            garbled.join(leviath_core::files::RUN_FILE),
            b"not a run file",
        )
        .expect("garbage written");
        let failure = read::spec("garbled").await.expect_err("it does not read");
        assert_eq!(failure.code(), "INTERNAL");

        let shut = runs.join("shut");
        std::fs::create_dir_all(shut.join(leviath_core::files::RUN_FILE)).expect("a directory");
        let failure = read::spec("shut").await.expect_err("it does not open");
        assert_eq!(failure.code(), "INTERNAL");
        let state: AppState = state_with_agent_paths(Vec::new());
        let codes = [
            read::state(&state, "shut", None).await.err(),
            read::deltas("shut", None, None).await.err(),
            read::graph("shut").await.err(),
        ];
        for failure in codes {
            assert_eq!(failure.map(|f| f.code()), Some("INTERNAL"));
        }
    })
    .await;
}

/// A step that will not decode fails what reads past it, and leaves the spec
/// readable.
#[tokio::test]
async fn a_step_that_will_not_decode_fails_what_reads_it() {
    crate::runstate::with_isolated_runs_dir_async("graphql-runfile-step", |_base| async move {
        let runs = crate::runstate::runs_dir();
        let run_id = walked(&runs);
        let path = file_of(&runs, &run_id);
        let mut bytes = std::fs::read(&path).expect("the file");
        bytes.extend(encode(FrameKind::Delta, &(4u64,)).expect("a frame encodes"));
        std::fs::write(&path, bytes).expect("the file is rewritten");
        let state: AppState = state_with_agent_paths(Vec::new());

        assert!(read::spec(&run_id).await.expect("the spec reads").is_some());
        let codes = [
            read::state(&state, &run_id, None).await.err(),
            read::deltas(&run_id, Some(4), Some(4)).await.err(),
            read::graph(&run_id).await.err(),
        ];
        for failure in codes {
            assert_eq!(failure.map(|f| f.code()), Some("INTERNAL"));
        }
    })
    .await;
}

/// A step that will not decode behind a checkpoint that does still leaves the
/// state readable, and fails the graph, which reads every step.
#[tokio::test]
async fn a_bad_step_behind_a_checkpoint_fails_only_what_reads_it() {
    crate::runstate::with_isolated_runs_dir_async("graphql-runfile-behind", |_base| async move {
        let runs = crate::runstate::runs_dir();
        let run_id = walked(&runs);
        let path = file_of(&runs, &run_id);
        let mut last = RunFileReader::read(&path)
            .expect("the run file reads")
            .latest_state()
            .expect("a state");
        last.seq = 4;
        let mut bytes = std::fs::read(&path).expect("the file");
        bytes.extend(encode(FrameKind::Delta, &(4u64,)).expect("a frame encodes"));
        bytes.extend(encode(FrameKind::State, &last).expect("a frame encodes"));
        std::fs::write(&path, bytes).expect("the file is rewritten");
        let state: AppState = state_with_agent_paths(Vec::new());

        let now = read::state(&state, &run_id, None)
            .await
            .expect("the state reads");
        assert_eq!(now.map(|s| s.seq), Some(4));
        let failure = read::graph(&run_id)
            .await
            .expect_err("a step does not decode");
        assert_eq!(failure.code(), "INTERNAL");
    })
    .await;
}

/// A root handing out one execution.
struct OneCall {
    execution: ToolExecution,
}

#[async_graphql::Object]
impl OneCall {
    /// The execution under test.
    async fn execution(&self) -> &ToolExecution {
        &self.execution
    }
}

/// What `definition` answers for a call to `tool` that the journal of
/// `run_id` recorded in the stage at `stage_index`.
async fn definition_of(run_id: &str, stage_index: usize, tool: &str) -> serde_json::Value {
    let execution = ToolExecution {
        run_id: run_id.to_string(),
        record: leviath_runtime::runfile::history::Execution {
            id: "x1".to_string(),
            call_id: "c1".to_string(),
            tool: tool.to_string(),
            arguments: "{}".to_string(),
            stage_index,
            iteration: 1,
            visit_id: String::new(),
            requested_by: String::new(),
            artifacts: Vec::new(),
            dispatched_at: 100,
            position: 1,
            ended_at: None,
            result_position: None,
            outcome: None,
        },
    };
    let answer = Schema::build(OneCall { execution }, EmptyMutation, EmptySubscription)
        .data(state_with_agent_paths(Vec::new()))
        .finish()
        .execute(Request::new(
            "{ execution { definition { name description schema source scriptDigest } } }",
        ))
        .await;
    serde_json::to_value(&answer).expect("the answer serializes")
}

/// An execution reads the tool as its own stage was given it, from the run's
/// file: the name, the words and the argument schema the model had.
#[tokio::test]
async fn an_execution_reads_the_tool_its_stage_was_given() {
    crate::runstate::with_isolated_runs_dir_async("graphql-runfile-offered", |_base| async move {
        let runs = crate::runstate::runs_dir();
        let run_id = walked(&runs);
        let spec = RunFileReader::read(&file_of(&runs, &run_id))
            .expect("the run file reads")
            .spec()
            .clone();
        let pinned = spec.stages[0]
            .tools
            .iter()
            .find(|tool| tool.name.as_str() == "read_file")
            .expect("the first stage is given read_file");

        let answer = definition_of(&run_id, 0, "read_file").await;
        assert_eq!(answer["errors"], serde_json::Value::Null, "{answer}");
        let definition = &answer["data"]["execution"]["definition"];
        assert_eq!(definition["name"], "read_file");
        assert_eq!(definition["description"], pinned.description.as_str());
        assert_eq!(&definition["schema"], pinned.schema.value());
        assert_eq!(definition["source"], "BUILTIN");
        assert_eq!(definition["scriptDigest"], serde_json::Value::Null);

        // A name the stage was not given has no definition there, though a
        // later stage is given it.
        let elsewhere = definition_of(&run_id, 0, "write_file").await;
        assert_eq!(elsewhere["errors"], serde_json::Value::Null, "{elsewhere}");
        assert!(elsewhere["data"]["execution"]["definition"].is_null());
        let later = definition_of(&run_id, 1, "write_file").await;
        assert_eq!(
            later["data"]["execution"]["definition"]["name"],
            "write_file"
        );

        // The second stage is given the shell under one of its names, and a
        // call under the other finds the same definition.
        let shell = read::offered_tool(&run_id, 1, "shell")
            .await
            .expect("the spec reads")
            .expect("the shell, by its own name");
        let bash = read::offered_tool(&run_id, 1, "bash")
            .await
            .expect("the spec reads")
            .expect("the shell, by its other name");
        assert_eq!(shell.name, bash.name);
        assert_eq!(shell.description, bash.description);

        // A stage the graph does not have, and a run with no run file.
        let past = read::offered_tool(&run_id, 99, "read_file")
            .await
            .expect("the spec reads");
        assert!(past.is_none());
        let absent = definition_of("no-such-run", 0, "read_file").await;
        assert_eq!(absent["errors"], serde_json::Value::Null, "{absent}");
        assert!(absent["data"]["execution"]["definition"].is_null());
    })
    .await;
}

/// A run file that no longer reads fails the definition, rather than reading
/// as a call to a tool the stage was never given.
#[tokio::test]
async fn a_definition_read_from_a_broken_run_file_fails() {
    crate::runstate::with_isolated_runs_dir_async(
        "graphql-runfile-offered-bad",
        |_base| async move {
            let runs = crate::runstate::runs_dir();
            let run_id = walked(&runs);
            std::fs::write(file_of(&runs, &run_id), b"not a run file")
                .expect("the file is rewritten");

            let answer = definition_of(&run_id, 0, "read_file").await;
            assert!(answer["errors"].is_array(), "{answer}");
            assert!(answer["data"]["execution"]["definition"].is_null());
        },
    )
    .await;
}
