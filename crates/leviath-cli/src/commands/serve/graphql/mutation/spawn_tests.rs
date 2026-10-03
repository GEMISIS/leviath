//! Tests for `spawnRun` and `validateSpawn`: what reaches the daemon, every
//! union member, and every issue this server finds before the daemon is asked.

use async_graphql::{EmptySubscription, Request, Schema, Variables};
use leviath_runtime::control_socket::{ControlClient, ControlRequest, ControlResponse};
use leviath_runtime::spec::inputs::RawInput;
use leviath_runtime::spec::issues::{IssueCode, SpawnIssue, SpawnIssues, SpecPath};
use leviath_runtime::spec::launch::Unattended;
use leviath_runtime::spec::request::SpawnSource;

use super::super::Mutation;
use super::super::spawn_request::{
    AttachmentContentWrite, CallbackWrite, CodeRefWrite, DeliveryWrite, GraphDocument,
    InputEntryWrite, InputValueWrite, LaunchWrite, OutputArtifactWrite, OutputShapeWrite,
    SpawnAttachmentWrite, SpawnRunRequest, SpawnSourceWrite, UnattendedWrite,
};
use super::SpawnIssueCode;
use crate::commands::serve::graphql::filter::testkit::round_trip;
use crate::commands::serve::graphql::inputs::{BlueprintRef, KeyValueWrite, RegionRef};
use crate::commands::serve::graphql::mutation::attachments::Delivery;
use crate::commands::serve::graphql::query::Query;
use crate::commands::serve::graphql::scalars::{BigInt, Json};
use crate::commands::serve::graphql::types::manifest::output::ValidatorErrorPolicy;
use crate::commands::serve::testutil::{fake_daemon, no_daemon_client, state_with_agent_paths};
use crate::commands::serve::types::{AppState, ServeLimits};
use crate::runstate::{RunMeta, create_run};

/// A state talking to `control`, with these limits.
fn state(control: ControlClient, limits: ServeLimits) -> AppState {
    let mut state = state_with_agent_paths(Vec::new());
    state.control = control;
    state.limits = std::sync::Arc::new(limits);
    state
}

/// Run one document with `variables` against a schema over `state`.
async fn run(state: AppState, query: &str, variables: serde_json::Value) -> serde_json::Value {
    let schema = Schema::build(Query, Mutation::default(), EmptySubscription)
        .data(state)
        .finish();
    let answer = schema
        .execute(Request::new(query).variables(Variables::from_json(variables)))
        .await;
    serde_json::to_value(&answer).expect("the answer serializes")
}

/// The selection both mutations answer with, every member and every field.
const SPAWN: &str = "mutation Spawn($request: SpawnRunRequest!) {
  spawnRun(request: $request) {
    __typename
    ... on SpawnedOutput { runId run { id task } }
    ... on SpawnRejectedOutput {
      issues { path code message expected got hint known segments { kind name index } }
    }
  }
}";

/// The selection `validateSpawn` answers with.
const VALIDATE: &str = "mutation Check($request: SpawnRunRequest!) {
  validateSpawn(request: $request) {
    __typename
    ... on SpawnSummaryOutput {
      title entryStage workdir
      origin { kind blueprintName digest version path }
      stages { stage provider model tools }
      inputs { name value { __typename ... on TextValueOutput { text asText } } }
      launch { unattended profile allow maxDepth seedCommands captureModelInput }
    }
    ... on SpawnRejectedOutput { issues { path code message } }
  }
}";

/// A run record on disk under `id`.
fn record(id: &str) {
    let meta = RunMeta::new(
        id.to_string(),
        "coder".to_string(),
        "/agents/coder".to_string(),
        "fix it".to_string(),
        None,
        "/work".to_string(),
        1,
    );
    create_run(&meta).expect("run written");
}

/// Every field of the request reaches the daemon as the runtime's own type, and
/// the run comes back once its record is there.
#[tokio::test]
async fn a_spawn_carries_every_field_it_was_given() {
    crate::runstate::with_isolated_runs_dir_async("graphql-spawn-all", |_d| async move {
        let workdir = tempfile::tempdir().expect("a workdir");
        std::fs::write(workdir.path().join("hero.png"), b"\x89PNG\r\n\x1a\nbody")
            .expect("the attachment");
        let (control, _dir, _srv) = fake_daemon(|request| {
            let ControlRequest::Spawn { request } = request else {
                panic!("a spawn, not {request:?}");
            };
            let SpawnSource::Blueprint(reference) = &request.source else {
                panic!("a blueprint request");
            };
            assert_eq!(reference.name.as_str(), "coder");
            assert_eq!(
                reference
                    .digest
                    .as_ref()
                    .map(ToString::to_string)
                    .as_deref(),
                Some("ab".repeat(32).as_str()),
                "the pin travels, lowercased"
            );
            assert_eq!(request.inputs["task"], RawInput::Text("fix it".into()));
            assert_eq!(request.inputs["depth"], RawInput::Int(3));
            assert_eq!(request.inputs["ratio"], RawInput::Float(0.5));
            assert_eq!(request.inputs["deep"], RawInput::Bool(true));
            let RawInput::List(items) = &request.inputs["items"] else {
                panic!("a list");
            };
            assert_eq!(items.len(), 2);
            let RawInput::Record(fields) = &request.inputs["model"] else {
                panic!("a record");
            };
            assert_eq!(fields["model"], RawInput::Text("m".into()));
            assert_eq!(
                request.model.as_ref().map(ToString::to_string).as_deref(),
                Some("mock/gpt-mock")
            );
            assert_eq!(request.attachments.len(), 2);
            let png = &request.attachments[0];
            assert_eq!(png.name, "hero.png", "a path names itself");
            assert_eq!(png.region.as_ref().map(|r| r.as_str()), Some("plan"));
            assert_eq!(png.deliver, Some(leviath_core::mime::Delivery::StandIn));
            assert_eq!(png.caption.as_deref(), Some("the hero"));
            assert_eq!(
                png.mime_type.as_ref().map(|m| m.as_str()),
                Some("image/png")
            );
            assert_eq!(request.attachments[1].name, "notes.txt");
            assert_eq!(request.attachments[1].data.0, b"hello");
            let output = request.output.as_ref().expect("an output shape");
            assert_eq!(output.format.as_deref(), Some("json"));
            assert_eq!(output.artifacts.len(), 1);
            assert!(output.schema.is_some() && output.validator.is_some());
            assert_eq!(
                output.on_validator_error,
                Some(leviath_core::output::OnValidatorError::Accept)
            );
            assert_eq!(request.launch.unattended, Unattended::All);
            assert_eq!(request.launch.max_depth, Some(2));
            assert_eq!(request.launch.allow.len(), 1);
            assert!(!request.launch.seed_commands);
            assert!(request.launch.capture_model_input);
            let callback = request.delivery.callback.as_ref().expect("a callback");
            assert_eq!(callback.url.as_str(), "https://1.1.1.1/hook");
            assert_eq!(callback.secret.as_ref().map(|s| s.expose()), Some("shh"));
            assert_eq!(request.delivery.metadata["ticket"], "42");
            record("coder-1");
            ControlResponse::Spawned {
                run_id: "coder-1".to_string(),
                warnings: Default::default(),
            }
        });
        let answer = run(
            state(control, ServeLimits::default()),
            SPAWN,
            serde_json::json!({ "request": {
                "source": { "blueprint": { "name": "coder", "digest": "AB".repeat(32) } },
                "inputs": [
                    { "name": "task", "value": { "text": "fix it" } },
                    { "name": "depth", "value": { "int": 3 } },
                    { "name": "ratio", "value": { "float": 0.5 } },
                    { "name": "deep", "value": { "bool": true } },
                    { "name": "items", "value": { "list": [{ "text": "a" }, { "int": 2 }] } },
                    { "name": "model", "value": { "record": [
                        { "name": "provider", "value": { "text": "mock" } },
                        { "name": "model", "value": { "text": "m" } },
                    ] } },
                ],
                "attachments": [
                    {
                        "content": { "path": "hero.png" },
                        "mimeType": "image/png",
                        "region": { "name": "plan" },
                        "deliver": "STAND_IN",
                        "caption": "the hero",
                    },
                    { "name": "notes.txt", "content": { "base64": "aGVsbG8=" } },
                ],
                "model": "mock/gpt-mock",
                "output": {
                    "format": "json",
                    "instructions": "one object",
                    "example": "{}",
                    "schema": { "type": "object" },
                    "validator": { "inline": "true" },
                    "onValidatorError": "ACCEPT",
                    "overwriteArtifacts": true,
                    "artifacts": [{ "name": "out", "mimeType": "text/plain" }],
                },
                "workdir": workdir.path().to_string_lossy(),
                "launch": {
                    "unattended": { "all": true },
                    "allow": ["read_file"],
                    "maxDepth": 2,
                    "seedCommands": false,
                    "captureModelInput": true,
                },
                "delivery": {
                    "callback": { "url": "https://1.1.1.1/hook", "secret": "shh" },
                    "metadata": [{ "key": "ticket", "value": "42" }],
                },
            } }),
        )
        .await;
        assert_eq!(answer["errors"], serde_json::Value::Null, "{answer}");
        let spawned = &answer["data"]["spawnRun"];
        assert_eq!(spawned["__typename"], "SpawnedOutput");
        assert_eq!(spawned["runId"], "coder-1");
        assert_eq!(spawned["run"]["task"], "fix it");
    })
    .await;
}

/// A raw graph goes to the daemon as the graph, and a run whose record is not
/// written yet comes back as its id alone.
#[tokio::test]
async fn a_raw_graph_is_sent_whole_and_a_run_not_yet_written_is_null() {
    crate::runstate::with_isolated_runs_dir_async("graphql-spawn-raw", |_d| async move {
        let graph: leviath_runtime::spec::graph::RunGraph = toml::from_str(
            "title = \"raw\"\n\n[[stages]]\nname = \"only\"\n\n[layout]\n\
             total_budget_tokens = 100\nregions = [{ name = \"plan\", kind = \"pinned\", budget = 100 }]\n",
        )
        .expect("the graph parses");
        let sent = graph.clone();
        let (control, _dir, _srv) = fake_daemon(move |request| {
            let ControlRequest::Spawn { request } = request else {
                panic!("a spawn");
            };
            assert_eq!(request.source, SpawnSource::Raw(Box::new(sent.clone())));
            assert_eq!(request.launch.unattended, Unattended::Off);
            assert!(request.launch.seed_commands, "seed commands default on");
            let output = request.output.as_ref().expect("an output shape");
            assert_eq!(
                output.validator,
                Some(leviath_runtime::spec::graph::CodeRef::File(
                    "check.rhai".into()
                ))
            );
            assert_eq!(
                output.on_validator_error,
                Some(leviath_core::output::OnValidatorError::Reject)
            );
            ControlResponse::Spawned {
                run_id: "raw-1".to_string(),
                warnings: Default::default(),
            }
        });
        let answer = run(
            state(control, ServeLimits::default()),
            SPAWN,
            serde_json::json!({ "request": {
                "source": { "graph": serde_json::to_value(&graph).expect("a graph is JSON") },
                "launch": { "unattended": { "all": false } },
                "output": {
                    "validator": { "file": "check.rhai" },
                    "onValidatorError": "REJECT",
                },
            } }),
        )
        .await;
        let spawned = &answer["data"]["spawnRun"];
        assert_eq!(spawned["runId"], "raw-1", "{answer}");
        assert_eq!(spawned["run"], serde_json::Value::Null);
    })
    .await;
}

/// The daemon's refusal is the `SpawnRejectedOutput` member, every issue with
/// its path one step at a time.
#[tokio::test]
async fn a_spawn_the_daemon_refuses_answers_with_its_issues() {
    let issue = SpawnIssue::new(
        SpecPath::root().field("inputs").key("items").index(2),
        IssueCode::WrongType,
        "the value has the wrong type",
    )
    .expected("an integer")
    .got("text \"x\"")
    .hint("send a number")
    .known(["a", "b"]);
    let refused = SpawnIssues::from(issue);
    let (control, _dir, _srv) = fake_daemon(move |_| ControlResponse::Rejected {
        issues: refused.clone(),
    });
    let answer = run(
        state(control, ServeLimits::default()),
        SPAWN,
        serde_json::json!({ "request": { "source": { "blueprint": { "name": "coder" } } } }),
    )
    .await;
    let rejected = &answer["data"]["spawnRun"];
    assert_eq!(rejected["__typename"], "SpawnRejectedOutput", "{answer}");
    let issue = &rejected["issues"][0];
    assert_eq!(issue["path"], "inputs.items[2]");
    assert_eq!(issue["code"], "WRONG_TYPE");
    assert_eq!(issue["expected"], "an integer");
    assert_eq!(issue["got"], "text \"x\"");
    assert_eq!(issue["hint"], "send a number");
    assert_eq!(issue["known"], serde_json::json!(["a", "b"]));
    assert_eq!(
        issue["segments"],
        serde_json::json!([
            { "kind": "FIELD", "name": "inputs", "index": null },
            { "kind": "KEY", "name": "items", "index": null },
            { "kind": "INDEX", "name": null, "index": 2 },
        ])
    );
}

/// Everything wrong with a request that this server can see is in one answer,
/// and the daemon is never asked.
#[tokio::test]
async fn every_problem_this_server_finds_is_answered_at_once() {
    let workdir = tempfile::tempdir().expect("a workdir");
    std::fs::write(workdir.path().join("big.bin"), [7u8; 32]).expect("a big file");
    let limits = ServeLimits {
        request_limits: crate::commands::serve::request_limits::RequestLimits {
            max_upload_bytes: 8,
            ..Default::default()
        },
        ..Default::default()
    };
    let answer = run(
        state(no_daemon_client(), limits),
        SPAWN,
        serde_json::json!({ "request": {
            "source": { "blueprint": { "name": " padded ", "digest": "not-hex" } },
            "inputs": [
                { "name": "task", "value": { "text": "a" } },
                { "name": "task", "value": { "text": "b" } },
                { "name": "pair", "value": { "record": [
                    { "name": "x", "value": { "int": 1 } },
                    { "name": "x", "value": { "int": 2 } },
                ] } },
            ],
            "attachments": [
                { "content": { "path": "../escape.png" } },
                { "content": { "path": "missing.txt" } },
                { "content": { "path": "big.bin" } },
                { "content": { "base64": "!!" } },
                { "name": "huge", "content": { "base64": "AAAAAAAAAAAAAAAA" } },
                { "content": { "base64": "aGk=" }, "mimeType": "not a type",
                  "region": { "name": " padded" } },
            ],
            "model": "bad/",
            "output": { "artifacts": [{ "name": "a", "mimeType": "nope" }] },
            "workdir": workdir.path().to_string_lossy(),
            "launch": {
                "unattended": { "profile": "padded " },
                "allow": ["bad tool"],
                "maxDepth": 300,
            },
            "delivery": { "callback": { "url": "not a url" } },
        } }),
    )
    .await;
    let rejected = &answer["data"]["spawnRun"];
    assert_eq!(rejected["__typename"], "SpawnRejectedOutput", "{answer}");
    let found: Vec<(String, String)> = rejected["issues"]
        .as_array()
        .expect("issues")
        .iter()
        .map(|issue| {
            (
                issue["path"].as_str().unwrap_or_default().to_string(),
                issue["code"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    let wanted = [
        ("source.blueprint.name", "INVALID"),
        ("source.blueprint.digest", "INVALID"),
        ("inputs.task", "DUPLICATE"),
        ("inputs.pair.x", "DUPLICATE"),
        ("attachments[0].content.path", "NOT_ALLOWED"),
        ("attachments[1].content.path", "UNRESOLVABLE"),
        ("attachments[2].content.path", "OUT_OF_RANGE"),
        ("attachments[3].content.base64", "INVALID"),
        ("attachments[4].content.base64", "OUT_OF_RANGE"),
        ("attachments[5].name", "MISSING"),
        ("attachments[5].mime_type", "INVALID"),
        ("attachments[5].region", "INVALID"),
        ("model", "INVALID"),
        ("output.artifacts[0].mime_type", "INVALID"),
        ("launch.unattended.profile", "INVALID"),
        ("launch.allow[0]", "INVALID"),
        ("launch.max_depth", "OUT_OF_RANGE"),
        ("delivery.callback.url", "INVALID"),
    ];
    for (path, code) in wanted {
        assert!(
            found.iter().any(|(p, c)| p == path && c == code),
            "{path} {code} missing from {found:?}"
        );
    }
    assert_eq!(found.len(), wanted.len(), "{found:?}");
}

/// A graph that is not a graph, and a callback URL that is not a URL, are
/// issues too, in the order the request is read in.
#[tokio::test]
async fn a_graph_that_does_not_read_is_an_issue() {
    let answer = run(
        state(no_daemon_client(), ServeLimits::default()),
        SPAWN,
        serde_json::json!({ "request": {
            "source": { "graph": { "stages": "none" } },
            "delivery": { "callback": { "url": "not a url" }, "metadata": [] },
        } }),
    )
    .await;
    let issues = &answer["data"]["spawnRun"]["issues"];
    assert_eq!(issues[0]["path"], "source.graph", "{answer}");
    assert_eq!(issues[0]["code"], "INVALID");
    assert!(
        issues[0]["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("spawn-request")),
        "{answer}"
    );
    assert_eq!(issues[1]["path"], "delivery.callback.url");
    assert_eq!(issues[1]["code"], "INVALID");
}

/// This server's own refusals come from the service layer REST shares, beside
/// whatever the daemon finds, as issues rather than as an error.
#[tokio::test]
async fn this_servers_refusals_are_issues_beside_the_daemons() {
    let root = tempfile::tempdir().expect("a workdir root");
    let outside = tempfile::tempdir().expect("a workdir outside it");
    let limits = ServeLimits {
        workdir_root: Some(root.path().to_path_buf()),
        no_remote_yolo: true,
        ..Default::default()
    };
    let refused = SpawnIssues::from(SpawnIssue::new(
        SpecPath::root().field("inputs").key("task"),
        IssueCode::Missing,
        "the task is required",
    ));
    let (control, _dir, _srv) = fake_daemon(move |_| ControlResponse::Rejected {
        issues: refused.clone(),
    });
    let answer = run(
        state(control, limits),
        SPAWN,
        serde_json::json!({ "request": {
            "source": { "blueprint": { "name": "coder" } },
            "workdir": outside.path().to_string_lossy(),
            "launch": { "unattended": { "all": true } },
            "delivery": { "callback": { "url": "http://127.0.0.1/hook" } },
        } }),
    )
    .await;
    let issues = answer["data"]["spawnRun"]["issues"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let paths: Vec<&str> = issues
        .iter()
        .filter_map(|issue| issue["path"].as_str())
        .collect();
    for path in [
        "workdir",
        "launch.unattended",
        "delivery.callback.url",
        "inputs.task",
    ] {
        assert!(paths.contains(&path), "{path} missing: {answer}");
    }
}

/// A daemon that answers with something that is neither a run nor a refusal,
/// or is not there at all, is a GraphQL error carrying its code.
#[tokio::test]
async fn a_daemon_that_does_not_answer_with_a_run_is_an_error() {
    let blueprint =
        serde_json::json!({ "request": { "source": { "blueprint": { "name": "c" } } } });
    for (reply, code) in [
        (
            ControlResponse::Error {
                message: "the daemon is shutting down".to_string(),
            },
            "DAEMON_UNAVAILABLE",
        ),
        (ControlResponse::Ok { ok: true }, "INTERNAL"),
    ] {
        for document in [SPAWN, VALIDATE] {
            let reply = reply.clone();
            let (control, _dir, _srv) = fake_daemon(move |_| reply.clone());
            let answer = run(
                state(control, ServeLimits::default()),
                document,
                blueprint.clone(),
            )
            .await;
            assert_eq!(answer["errors"][0]["extensions"]["code"], code, "{answer}");
        }
    }
    for document in [SPAWN, VALIDATE] {
        let answer = run(
            state(no_daemon_client(), ServeLimits::default()),
            document,
            blueprint.clone(),
        )
        .await;
        assert_eq!(
            answer["errors"][0]["extensions"]["code"], "DAEMON_UNAVAILABLE",
            "{answer}"
        );
    }
}

/// A run in brief, as a dry run the daemon accepts answers with.
fn brief() -> leviath_runtime::spec::summary::SpawnSummary {
    use leviath_runtime::spec::inputs::{InputValue, InputValues};
    use leviath_runtime::spec::launch::LaunchPolicy;
    use leviath_runtime::spec::names::{BlueprintName, ModelId, ProviderName, StageName, ToolName};
    use leviath_runtime::spec::run_spec::SpecOrigin;
    use leviath_runtime::spec::summary::{SpawnSummary, StageSummary};

    SpawnSummary {
        title: "coder".into(),
        origin: SpecOrigin::BlueprintFile {
            path: leviath_runtime::spec::names::BlueprintPath::new(
                std::env::temp_dir().to_string_lossy(),
            )
            .expect("an absolute path"),
            name: BlueprintName::new("coder").expect("a name"),
            digest: None,
            version: "1.0.0".into(),
        },
        entry_stage: StageName::new("plan").expect("a stage"),
        stages: vec![StageSummary {
            stage: StageName::new("plan").expect("a stage"),
            provider: ProviderName::new("mock").expect("a provider"),
            model: ModelId::new("gpt-mock").expect("a model"),
            tools: vec![ToolName::new("read_file").expect("a tool")],
        }],
        inputs: InputValues(
            [(
                leviath_runtime::spec::names::InputName::new("task").expect("a name"),
                InputValue::Text("fix it".into()),
            )]
            .into(),
        ),
        launch: LaunchPolicy {
            unattended: Unattended::Profile(
                leviath_runtime::spec::names::ProfileName::new("safe").expect("a profile"),
            ),
            allow: vec![ToolName::new("shell").expect("a tool")],
            max_depth: 2,
            seed_commands: true,
            capture_model_input: false,
        },
        workdir: "/work".into(),
        warnings: Default::default(),
    }
}

/// A dry run answers with the run in brief, and the request it checks is the
/// one a spawn would send.
#[tokio::test]
async fn a_dry_run_answers_with_the_run_in_brief() {
    let summary = brief();
    let (control, _dir, _srv) = fake_daemon(move |request| {
        assert!(
            matches!(request, ControlRequest::ValidateSpawn { .. }),
            "a dry run, not {request:?}"
        );
        ControlResponse::Valid {
            summary: Box::new(summary.clone()),
        }
    });
    let answer = run(
        state(control, ServeLimits::default()),
        VALIDATE,
        serde_json::json!({ "request": { "source": { "blueprint": { "name": "coder" } } } }),
    )
    .await;
    let valid = &answer["data"]["validateSpawn"];
    assert_eq!(valid["__typename"], "SpawnSummaryOutput", "{answer}");
    assert_eq!(valid["title"], "coder");
    assert_eq!(valid["entryStage"], "plan");
    assert_eq!(valid["origin"]["kind"], "BLUEPRINT_FILE");
    assert_eq!(valid["origin"]["blueprintName"], "coder");
    assert_eq!(valid["origin"]["version"], "1.0.0");
    assert_eq!(valid["stages"][0]["tools"][0], "read_file");
    assert_eq!(valid["inputs"][0]["value"]["text"], "fix it");
    assert_eq!(valid["launch"]["unattended"], "PROFILE");
    assert_eq!(valid["launch"]["profile"], "safe");
    assert_eq!(valid["launch"]["maxDepth"], 2);
}

/// A dry run the daemon refuses, and one this server refuses before asking,
/// both answer with the issues.
#[tokio::test]
async fn a_dry_run_that_would_be_refused_says_why() {
    let refused = SpawnIssues::from(SpawnIssue::new(
        SpecPath::root().field("source"),
        IssueCode::Unresolvable,
        "no blueprint named \"coder\" is installed",
    ));
    let (control, _dir, _srv) = fake_daemon(move |_| ControlResponse::Rejected {
        issues: refused.clone(),
    });
    let answer = run(
        state(control, ServeLimits::default()),
        VALIDATE,
        serde_json::json!({ "request": { "source": { "blueprint": { "name": "coder" } } } }),
    )
    .await;
    let rejected = &answer["data"]["validateSpawn"];
    assert_eq!(rejected["__typename"], "SpawnRejectedOutput", "{answer}");
    assert_eq!(rejected["issues"][0]["code"], "UNRESOLVABLE");

    let local = run(
        state(no_daemon_client(), ServeLimits::default()),
        VALIDATE,
        serde_json::json!({ "request": { "source": { "blueprint": { "name": "" } } } }),
    )
    .await;
    assert_eq!(
        local["data"]["validateSpawn"]["issues"][0]["path"], "source.blueprint.name",
        "{local}"
    );

    let pinned = run(
        state(no_daemon_client(), ServeLimits::default()),
        VALIDATE,
        serde_json::json!({ "request": { "source": {
            "blueprint": { "name": "coder", "digest": "not-hex" } } } }),
    )
    .await;
    let issues = &pinned["data"]["validateSpawn"]["issues"];
    assert_eq!(issues[0]["path"], "source.blueprint.digest", "{pinned}");
    assert_eq!(issues.as_array().map(Vec::len), Some(1));
}

/// An issue's path reads in this schema's field names: a raw graph is
/// `source.graph`, an attachment's bytes its `content`; anything else is
/// left alone.
#[test]
fn issue_paths_name_this_schemas_fields() {
    let shown = |path: SpecPath| {
        let issue = super::SpawnIssue::from(&SpawnIssue::new(path, IssueCode::Invalid, "x"));
        let names: Vec<Option<String>> = issue.segments.iter().map(|s| s.name.clone()).collect();
        (issue.path, names)
    };
    let raw = SpecPath::root()
        .field("source")
        .field("raw")
        .field("edges")
        .index(0)
        .field("to");
    let (path, names) = shown(raw);
    assert_eq!(path, "source.graph.edges[0].to");
    assert_eq!(names[1].as_deref(), Some("graph"));
    let data = SpecPath::root().field("attachments").index(1).field("data");
    assert_eq!(shown(data).0, "attachments[1].content");
    for kept in [
        SpecPath::root().field("source").field("blueprint"),
        SpecPath::root().field("attachments").index(1).field("name"),
        SpecPath::root().field("inputs").key("raw"),
        SpecPath::root(),
    ] {
        assert_eq!(shown(kept.clone()).0, kept.to_string());
    }
}

/// Every issue code the runtime has is one this schema can say.
#[test]
fn every_issue_code_crosses_over() {
    let pairs = [
        (IssueCode::Missing, SpawnIssueCode::Missing),
        (IssueCode::Unknown, SpawnIssueCode::Unknown),
        (IssueCode::WrongType, SpawnIssueCode::WrongType),
        (IssueCode::OutOfRange, SpawnIssueCode::OutOfRange),
        (IssueCode::Invalid, SpawnIssueCode::Invalid),
        (IssueCode::Duplicate, SpawnIssueCode::Duplicate),
        (IssueCode::Dangling, SpawnIssueCode::Dangling),
        (IssueCode::Conflict, SpawnIssueCode::Conflict),
        (IssueCode::NotAllowed, SpawnIssueCode::NotAllowed),
        (IssueCode::Unresolvable, SpawnIssueCode::Unresolvable),
        (IssueCode::Unavailable, SpawnIssueCode::Unavailable),
        (IssueCode::Changed, SpawnIssueCode::Changed),
    ];
    for (code, want) in pairs {
        assert_eq!(SpawnIssueCode::from(code), want);
    }
}

/// Every write shape reads back from its own value, and refuses a field of
/// the wrong type.
#[test]
fn every_spawn_write_shape_round_trips() {
    let text = |s: &str| InputValueWrite::Text(s.to_string());
    for value in [
        text("a"),
        InputValueWrite::Bool(true),
        InputValueWrite::Int(BigInt(3)),
        InputValueWrite::Float(0.5),
        InputValueWrite::List(vec![text("a")]),
        InputValueWrite::Record(vec![InputEntryWrite {
            name: "x".to_string(),
            value: text("y"),
        }]),
    ] {
        round_trip(&value);
    }
    round_trip(&InputEntryWrite {
        name: "task".to_string(),
        value: text("t"),
    });
    round_trip(&SpawnSourceWrite::Blueprint(BlueprintRef {
        name: "coder".to_string(),
        digest: Some("ab".repeat(32)),
    }));
    round_trip(&SpawnSourceWrite::Graph(GraphDocument(
        serde_json::json!({ "stages": [] }),
    )));
    for content in [
        AttachmentContentWrite::Path("a.png".to_string()),
        AttachmentContentWrite::Base64("aGk=".to_string()),
    ] {
        round_trip(&content);
    }
    for code in [
        CodeRefWrite::File("check.rhai".to_string()),
        CodeRefWrite::Inline("true".to_string()),
    ] {
        round_trip(&code);
    }
    for unattended in [
        UnattendedWrite::All(true),
        UnattendedWrite::Profile("safe".to_string()),
    ] {
        round_trip(&unattended);
    }
    round_trip(&SpawnAttachmentWrite {
        name: Some("a.png".to_string()),
        content: AttachmentContentWrite::Path("a.png".to_string()),
        mime_type: Some("image/png".to_string()),
        region: Some(RegionRef {
            name: "plan".to_string(),
        }),
        deliver: Some(Delivery::Native),
        caption: Some("c".to_string()),
    });
    round_trip(&OutputArtifactWrite {
        name: "out".to_string(),
        mime_type: "text/plain".to_string(),
        required: true,
        description: Some("d".to_string()),
    });
    round_trip(&OutputShapeWrite {
        format: Some("json".to_string()),
        instructions: Some("i".to_string()),
        example: Some("e".to_string()),
        schema: Some(Json(serde_json::json!({}))),
        validator: Some(CodeRefWrite::Inline("true".to_string())),
        on_validator_error: Some(ValidatorErrorPolicy::Reject),
        overwrite_artifacts: Some(false),
        artifacts: Some(Vec::new()),
    });
    round_trip(&LaunchWrite {
        unattended: Some(UnattendedWrite::All(false)),
        allow: Some(vec!["shell".to_string()]),
        max_depth: Some(1),
        seed_commands: true,
        capture_model_input: false,
    });
    round_trip(&CallbackWrite {
        url: "https://example.com".to_string(),
        secret: Some("s".to_string()),
    });
    round_trip(&DeliveryWrite {
        callback: None,
        metadata: Some(vec![KeyValueWrite {
            key: "k".to_string(),
            value: "v".to_string(),
        }]),
    });
    round_trip(&SpawnRunRequest {
        source: SpawnSourceWrite::Blueprint(BlueprintRef {
            name: "coder".to_string(),
            digest: None,
        }),
        inputs: Some(Vec::new()),
        attachments: Some(Vec::new()),
        model: Some("m".to_string()),
        output: None,
        workdir: Some("/work".to_string()),
        launch: None,
        delivery: None,
    });
}

/// A value that does not read (a depth past 255) is listed beside what the
/// daemon finds in the rest of the request, for a spawn and a dry run alike,
/// and stands alone when the rest is fine.
#[tokio::test]
async fn a_value_that_does_not_read_is_listed_beside_the_daemons_issues() {
    let refused = SpawnIssues::from(SpawnIssue::new(
        SpecPath::root().field("inputs").key("task"),
        IssueCode::WrongType,
        "the value has the wrong type",
    ));
    let request = serde_json::json!({ "request": {
        "source": { "blueprint": { "name": "coder" } },
        "launch": { "maxDepth": 999 },
    } });
    for (document, field) in [(SPAWN, "spawnRun"), (VALIDATE, "validateSpawn")] {
        let refused = refused.clone();
        let (control, _dir, _srv) = fake_daemon(move |request| {
            assert!(
                matches!(request, ControlRequest::ValidateSpawn { .. }),
                "only ever checked, never started: {request:?}"
            );
            ControlResponse::Rejected {
                issues: refused.clone(),
            }
        });
        let answer = run(
            state(control, ServeLimits::default()),
            document,
            request.clone(),
        )
        .await;
        let paths: Vec<&str> = answer["data"][field]["issues"]
            .as_array()
            .expect("issues")
            .iter()
            .filter_map(|issue| issue["path"].as_str())
            .collect();
        assert_eq!(paths, ["launch.max_depth", "inputs.task"], "{answer}");
    }
    let (control, _dir, _srv) = fake_daemon(move |_| ControlResponse::Valid {
        summary: Box::new(brief()),
    });
    let answer = run(state(control, ServeLimits::default()), SPAWN, request).await;
    let paths: Vec<&str> = answer["data"]["spawnRun"]["issues"]
        .as_array()
        .expect("issues")
        .iter()
        .filter_map(|issue| issue["path"].as_str())
        .collect();
    assert_eq!(paths, ["launch.max_depth"], "{answer}");
}

#[test]
fn a_warning_reads_as_its_own_code() {
    assert_eq!(
        super::SpawnIssueCode::from(leviath_runtime::spec::issues::IssueCode::MayNeverFinish),
        super::SpawnIssueCode::MayNeverFinish
    );
}
